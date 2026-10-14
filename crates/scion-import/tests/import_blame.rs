//! SPEC 19.6: the Blame RPC on imported real corpora equals `git blame --porcelain` line
//! origins (commit mapped through `git_mapping`, plus the origin line number) for every file
//! without renames at the imported tip (P29). Skipped when the git CLI is absent.
//!
//! - Files whose history git follows through a rename (a porcelain `filename` differing from
//!   the path) are out of scope and skipped.
//! - Any other disagreement must be documented in `tests/BLAME_EXCEPTIONS.md` with an upper
//!   bound on the number of differing lines, and every differing line must still be a valid
//!   attribution on the Scion side: the version of the file at Scion's origin changeset holds
//!   the identical line at Scion's origin line number (the two tools only chose different
//!   alignments among equal lines).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod import_support;

use import_support::*;
use scion_core::{Mode, ObjectId};
use scion_import::testing::{git_commits, git_files, options, resolve_git};
use scion_proto::convert::parse_id;
use scion_proto::v1;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

/// Documented exceptions: `(corpus, path) -> max differing lines`, from table rows of the
/// form ``| `corpus` | `path` | max | reason |`` in `tests/BLAME_EXCEPTIONS.md`.
fn exceptions() -> BTreeMap<(String, String), usize> {
    let md = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/BLAME_EXCEPTIONS.md"),
    )
    .expect("tests/BLAME_EXCEPTIONS.md");
    md.lines()
        .filter_map(|l| {
            let cells: Vec<&str> = l.split('|').map(str::trim).collect();
            let corpus = cells.get(1)?.strip_prefix('`')?.strip_suffix('`')?;
            let path = cells.get(2)?.strip_prefix('`')?.strip_suffix('`')?;
            let max: usize = cells.get(3)?.parse().ok()?;
            Some(((corpus.to_owned(), path.to_owned()), max))
        })
        .collect()
}

/// Per final line: `(origin commit hex, origin line, 1-based)`, plus every `filename` seen.
fn git_blame(repo: &Path, rev: &str, path: &str) -> (Vec<(String, u32)>, BTreeSet<String>) {
    let out = git_out(repo, &["blame", "--porcelain", rev, "--", path]);
    let mut lines = Vec::new();
    let mut names = BTreeSet::new();
    let mut cur: Option<(String, u32)> = None;
    for l in out.split(|b| *b == b'\n') {
        if l.first() == Some(&b'\t') {
            let (sha, orig) = cur.take().expect("content line before a header");
            lines.push((sha, orig));
            continue;
        }
        let text = String::from_utf8_lossy(l);
        let mut f = text.split(' ');
        let first = f.next().unwrap_or_default();
        if first.len() == 40 && first.bytes().all(|b| b.is_ascii_hexdigit()) {
            let orig: u32 = f.next().unwrap().parse().unwrap();
            cur = Some((first.to_owned(), orig));
        } else if let Some(name) = text.strip_prefix("filename ") {
            names.insert(name.to_owned());
        }
    }
    (lines, names)
}

/// Per line of the Blame RPC answer: `(origin changeset, origin line, 0-based)`.
async fn scion_blame(
    c: &scion_proto::ScionClient,
    cs: &ObjectId,
    path: &str,
) -> Vec<(ObjectId, u32)> {
    let resp = c
        .query()
        .blame(v1::BlameRequest {
            changeset: idb(cs),
            path: path.as_bytes().to_vec(),
        })
        .await
        .unwrap()
        .into_inner();
    let mut out = Vec::new();
    for r in resp.ranges {
        assert_eq!(r.start_line as usize, out.len(), "ranges are contiguous");
        let origin = parse_id(&r.origin_changeset, "origin").unwrap();
        for k in 0..r.line_count {
            out.push((origin, r.origin_start_line + k));
        }
    }
    out
}

fn lines_of(content: &[u8]) -> Vec<Vec<u8>> {
    content.split(|b| *b == b'\n').map(<[u8]>::to_vec).collect()
}

async fn blame_corpus(name: &str) {
    if !scion_testkit::git::git_available() {
        println!("SKIP: git CLI absent; blame differential for {name} skipped");
        return;
    }
    let s = server().await;
    let c = importer_client(&s).await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = unpack(name, tmp.path());
    let skip = name == "synthetic-edge";
    let rep = scion_import::import(&c, &options(&repo, "HEAD", "imp/blame", skip))
        .await
        .unwrap();
    let tip_cs = rep.final_target.unwrap();
    let commits = git_commits(&repo, "HEAD").unwrap();
    let tip = commits.last().unwrap().id.clone();
    let mut to_cs: HashMap<String, ObjectId> = HashMap::new();
    for commit in &commits {
        to_cs.insert(
            commit.id.clone(),
            resolve_git(&c, &commit.id).await.unwrap().unwrap(),
        );
    }
    let to_git: HashMap<ObjectId, String> = to_cs.iter().map(|(g, c)| (*c, g.clone())).collect();

    let allowed = exceptions();
    let mut compared = 0;
    let mut renamed = Vec::new();
    let mut failures = Vec::new();
    let mut excepted_hit = Vec::new();
    for (path, file) in git_files(&repo, &tip).unwrap().files {
        if file.mode == Mode::Symlink {
            continue;
        }
        let path = String::from_utf8(path).expect("utf-8 paths in the corpora");
        let (git_lines, names) = git_blame(&repo, &tip, &path);
        if names.iter().any(|n| *n != path) {
            renamed.push(path);
            continue;
        }
        let want: Vec<(ObjectId, u32)> = git_lines
            .iter()
            .map(|(sha, orig)| (to_cs[sha], orig - 1))
            .collect();
        let got = scion_blame(&c, &tip_cs, &path).await;
        compared += 1;
        assert_eq!(want.len(), got.len(), "{name} {path}: line counts differ");
        let text = lines_of(&file.content);
        let mut differing = 0;
        for (i, (a, b)) in want.iter().zip(got.iter()).enumerate() {
            if a == b {
                continue;
            }
            differing += 1;
            // Scion's attribution must be valid: the origin version has the same line there.
            let origin_git = &to_git[&b.0];
            let origin_files = git_files(&repo, origin_git).unwrap();
            let origin_text = lines_of(
                &origin_files
                    .files
                    .get(path.as_bytes())
                    .unwrap_or_else(|| panic!("{name} {path} missing at {origin_git}"))
                    .content,
            );
            let line = text.get(i).cloned().unwrap_or_default();
            println!(
                "{name} {path} line {} {:?}: git {}:{} scion {}:{}",
                i + 1,
                String::from_utf8_lossy(&line),
                &to_git[&a.0][..10],
                a.1 + 1,
                &origin_git[..10],
                b.1 + 1
            );
            if origin_text.get(b.1 as usize) != Some(&line) {
                failures.push(format!(
                    "{name} {path} line {}: Scion origin {origin_git}:{} does not hold that line",
                    i + 1,
                    b.1 + 1
                ));
            }
        }
        if differing == 0 {
            continue;
        }
        match allowed.get(&(name.to_owned(), path.clone())) {
            Some(max) if differing <= *max => excepted_hit.push((path, differing)),
            Some(max) => failures.push(format!(
                "{name} {path}: {differing} lines differ, BLAME_EXCEPTIONS.md allows {max}"
            )),
            None => failures.push(format!(
                "{name} {path}: {differing} of {} lines differ and the file is not in \
                 BLAME_EXCEPTIONS.md",
                want.len()
            )),
        }
    }
    println!(
        "{name}: compared {compared} files; renamed (skipped) {renamed:?}; documented \
         exceptions hit {excepted_hit:?}"
    );
    assert!(compared > 0);
    assert!(
        failures.is_empty(),
        "blame differs from git blame: {failures:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn blame_matches_git_blame_cfg_if() {
    blame_corpus("cfg-if").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn blame_matches_git_blame_itoa() {
    blame_corpus("itoa").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn blame_matches_git_blame_synthetic_edge() {
    blame_corpus("synthetic-edge").await;
}
