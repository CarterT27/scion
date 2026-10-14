//! SPEC 19.14 differential fidelity over every commit of every vendored bundle (two real
//! histories and the synthetic edge history), plus idempotent re-import (P29).
//!
//! Every ref of each bundle is imported into its own Scion ref. Then, for every commit:
//! - `check_fidelity_commits`: tree file set, modes and contents (gix tree walk on the git
//!   side), Meta fields byte for byte, extension tags 2 and 4, parents, change id and
//!   `git_mapping` in both directions;
//! - an independent comparison against `scion_testkit::git::ls_tree` (P09's gix reader);
//! - when the git CLI is present, against `git ls-tree -r` plus `git cat-file` contents.
//!
//! Documented exclusions: skipped gitlinks and empty git trees (SPEC 14).

mod import_support;

use import_support::*;
use scion_core::{Mode, ObjectId};
use scion_import::map_mode;
use scion_import::testing::{check_fidelity_commits, git_commits_for_refs, options};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::Stdio;

/// One expected file: Scion mode and content.
type Expected = BTreeMap<Vec<u8>, (Mode, Vec<u8>)>;

/// Expected files from `git ls-tree -r -z` + `git cat-file --batch` (git CLI).
fn git_cli_trees(repo: &Path, commits: &[String]) -> HashMap<String, Expected> {
    let mut listings: HashMap<String, Vec<(Vec<u8>, u32, String)>> = HashMap::new();
    let mut oids: BTreeSet<String> = BTreeSet::new();
    for c in commits {
        let out = git_out(repo, &["ls-tree", "-r", "-z", c]);
        let mut entries = Vec::new();
        for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let tab = rec.iter().position(|b| *b == b'\t').expect("tab");
            let meta = std::str::from_utf8(&rec[..tab]).unwrap();
            let path = rec[tab + 1..].to_vec();
            let mut f = meta.split(' ');
            let mode = u32::from_str_radix(f.next().unwrap(), 8).unwrap();
            let ty = f.next().unwrap();
            let oid = f.next().unwrap().to_owned();
            if ty == "commit" {
                continue; // gitlink: documented exclusion
            }
            assert_eq!(ty, "blob");
            oids.insert(oid.clone());
            entries.push((path, mode, oid));
        }
        listings.insert(c.clone(), entries);
    }
    // One `git cat-file --batch` for every distinct blob.
    let mut child = git(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git cat-file");
    let mut stdin = child.stdin.take().unwrap();
    let input: String = oids.iter().map(|o| format!("{o}\n")).collect();
    let writer = std::thread::spawn(move || {
        stdin.write_all(input.as_bytes()).unwrap();
    });
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut contents: HashMap<String, Vec<u8>> = HashMap::new();
    for _ in 0..oids.len() {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let mut f = header.trim_end().split(' ');
        let oid = f.next().unwrap().to_owned();
        assert_eq!(f.next(), Some("blob"), "cat-file header {header:?}");
        let size: usize = f.next().unwrap().parse().unwrap();
        let mut buf = vec![0u8; size + 1];
        reader.read_exact(&mut buf).unwrap();
        buf.pop();
        contents.insert(oid, buf);
    }
    writer.join().unwrap();
    assert!(child.wait().unwrap().success());
    listings
        .into_iter()
        .map(|(c, entries)| {
            let m = entries
                .into_iter()
                .map(|(p, mode, oid)| {
                    let mode = map_mode(mode).unwrap().expect("blob mode");
                    (p, (mode, contents[&oid].clone()))
                })
                .collect();
            (c, m)
        })
        .collect()
}

/// Expected files from P09's gix reader (`scion_testkit::git::ls_tree`).
fn testkit_tree(repo: &Path, commit: &str) -> Expected {
    scion_testkit::git::ls_tree(repo, commit)
        .unwrap()
        .into_iter()
        .filter_map(|e| {
            let content = e.content?; // gitlinks carry no content: excluded
            Some((e.path, (map_mode(e.mode).unwrap().unwrap(), content)))
        })
        .collect()
}

async fn fidelity_for_bundle(name: &str, skip_submodules: bool) {
    let s = server().await;
    let c = importer_client(&s).await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = unpack(name, tmp.path());
    let refs = bundle_refs(name);
    assert!(!refs.is_empty());

    let mut imported = 0;
    for r in &refs {
        let rep = scion_import::import(&c, &options(&repo, r, &target_for(r), skip_submodules))
            .await
            .unwrap_or_else(|e| panic!("import {name} {r}: {e}"));
        assert_eq!(rep.imported + rep.skipped_existing, rep.commits_seen);
        imported += rep.imported;
        let (target, version) = resolve_ref(&c, &target_for(r)).await.unwrap();
        assert_eq!(Some(target), rep.final_target);
        assert_eq!(version, rep.ref_version);
    }

    // Every commit of the bundle, as listed by P09's reader, is covered by the ref walks.
    let commits = git_commits_for_refs(&repo, &refs).unwrap();
    let ids: BTreeSet<String> = commits.iter().map(|c| c.id.clone()).collect();
    let all: BTreeSet<String> = scion_testkit::git::commits(&repo)
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, all, "{name}: commit sets differ");
    assert_eq!(
        imported as usize,
        commits.len(),
        "{name}: one changeset per commit"
    );

    let f = check_fidelity_commits(&c, &repo, &commits).await.unwrap();
    assert!(
        f.mismatches.is_empty(),
        "{name}: {} mismatches, first: {:?}",
        f.mismatches.len(),
        f.mismatches.iter().take(10).collect::<Vec<_>>()
    );
    assert_eq!(f.commits_checked, commits.len());
    assert!(f.files_checked > 0 && f.blobs_compared > 0);

    // Independent comparison sides.
    let cli = scion_testkit::git::git_available();
    if !cli {
        println!("SKIP: git CLI absent; `git ls-tree -r` differential skipped for {name}");
    }
    let cli_trees = if cli {
        git_cli_trees(
            &repo,
            &commits.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
        )
    } else {
        HashMap::new()
    };
    let mut content_cache: HashMap<ObjectId, Vec<u8>> = HashMap::new();
    for commit in &commits {
        let cs = mapped(&c, &commit.id).await;
        let mut reader = scion_import::testing::ScionReader::new(&c);
        let changeset = reader.changeset(&cs).await.unwrap();
        let files = reader.files(&changeset.root_tree).await.unwrap();
        let mut got: Expected = BTreeMap::new();
        for (p, file) in files {
            let content = match content_cache.get(&file.id) {
                Some(b) => b.clone(),
                None => {
                    let b = reader.blob(&file.id).await.unwrap().to_vec();
                    content_cache.insert(file.id, b.clone());
                    b
                }
            };
            got.insert(p, (file.mode, content));
        }
        assert_eq!(
            got,
            testkit_tree(&repo, &commit.id),
            "{name} {}: Scion tree != gix ls_tree",
            commit.id
        );
        if cli {
            assert_eq!(
                &got, &cli_trees[&commit.id],
                "{name} {}: Scion tree != git ls-tree -r",
                commit.id
            );
        }
    }

    // Re-import is idempotent: no new rows anywhere.
    let mem = s.mem.as_ref().expect("in-memory storage");
    let before = mem.meta.table_counts();
    for r in &refs {
        let rep = scion_import::import(&c, &options(&repo, r, &target_for(r), skip_submodules))
            .await
            .unwrap();
        assert_eq!(rep.imported, 0, "{name} {r}: re-import created changesets");
        assert_eq!(rep.skipped_existing, rep.commits_seen);
        assert_eq!(rep.blobs_uploaded, 0);
    }
    assert_eq!(
        mem.meta.table_counts(),
        before,
        "{name}: re-import added rows"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fidelity_cfg_if_every_commit() {
    fidelity_for_bundle("cfg-if", false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fidelity_itoa_every_commit() {
    fidelity_for_bundle("itoa", false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn fidelity_synthetic_edge_every_commit() {
    fidelity_for_bundle("synthetic-edge", true).await;
}
