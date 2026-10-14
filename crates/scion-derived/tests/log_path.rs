//! SPEC 19.6 `history` / 7.4 `Log(path)` and SPEC 19.13 `--all-versions` (P24):
//!
//! - `log::log_path` equals the reference model's TREESAME history (`MRepo::log_path`) for
//!   every path of generated corpora with merges, at every commit, with and without
//!   `first_parent_only`, in the deterministic order (generation descending, ties by
//!   descending changeset id);
//! - paging: concatenated pages equal the full walk, the token is bound to its query;
//! - `versions::all_versions` equals `MRepo::all_versions` (distinct blobs, first-seen order).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod history_support;

use history_support::*;
use scion_core::subcode::BAD_PAGE_TOKEN;
use scion_core::{ErrorCode, Kind, ObjectId, test_support::proptest_cases};
use scion_derived::log::{PathLogPage, log_path};
use scion_derived::versions::all_versions;
use scion_object::{Extensions, RepoPath};
use scion_refmodel::MTree;
use scion_testkit::{CorpusSpec, GenRepo, generate};
use std::collections::{BTreeSet, HashMap};

fn spec(seed: u64) -> CorpusSpec {
    CorpusSpec {
        files: 30,
        depth: 3,
        fanout: 3,
        history_len: 30,
        merge_ratio: 0.25,
        octopus_ratio: 0.08,
        seed,
        ..CorpusSpec::default()
    }
}

async fn generations(s: &Stack, ids: &[ObjectId]) -> HashMap<ObjectId, u64> {
    let mut out = HashMap::new();
    for id in ids {
        out.insert(*id, s.h.changeset(id).await.unwrap().generation);
    }
    out
}

/// The model's log mapped to ids, in production order (generation desc, id desc).
fn model_log(
    model: &scion_refmodel::MRepo,
    ids: &[ObjectId],
    gens: &HashMap<ObjectId, u64>,
    c: usize,
    path: &[u8],
    first_parent_only: bool,
) -> Vec<ObjectId> {
    let mut v: Vec<ObjectId> = model
        .log_path(c, path, first_parent_only)
        .into_iter()
        .map(|i| ids[i])
        .collect();
    v.sort_by(|a, b| gens[b].cmp(&gens[a]).then(b.cmp(a)));
    v
}

async fn full_log(s: &Stack, from: &ObjectId, path: &RepoPath, fp: bool) -> Vec<ObjectId> {
    let page = log_path(&s.engine, from, path, fp, usize::MAX, None)
        .await
        .unwrap();
    assert!(page.next_page.is_none());
    page.changesets
}

/// Every path any commit of `repo` ever held (files, directories, root).
fn every_path(repo: &GenRepo) -> BTreeSet<Vec<u8>> {
    let mut out = BTreeSet::new();
    for c in &repo.commits {
        out.extend(all_paths(&c.tree));
    }
    out
}

async fn check_log_corpus(s: &Stack, repo: &GenRepo, ids: &[ObjectId]) -> usize {
    let model = model_of(repo);
    let gens = generations(s, ids).await;
    let paths = every_path(repo);
    let mut checked = 0;
    for c in 0..repo.commits.len() {
        for path in &paths {
            let rp = RepoPath::parse(path).unwrap();
            for fp in [false, true] {
                let got = full_log(s, &ids[c], &rp, fp).await;
                let want = model_log(&model, ids, &gens, c, path, fp);
                assert_eq!(
                    got,
                    want,
                    "commit {c}, path {:?}, first_parent_only {fp}",
                    String::from_utf8_lossy(path)
                );
                checked += 1;
            }
        }
    }
    checked
}

#[tokio::test(flavor = "multi_thread")]
async fn log_path_matches_refmodel_on_generated_corpora() {
    // SCION_PROPTEST_CASES scales the number of seeds (default 2).
    let seeds = u64::from(proptest_cases(2).max(1));
    let mut total = 0;
    for seed in 0..seeds {
        let repo = generate(&spec(seed));
        let s = stack();
        let ids = s.load(&repo).await;
        total += check_log_corpus(&s, &repo, &ids).await;
    }
    assert!(total > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn pages_concatenate_to_the_full_walk() {
    let repo = generate(&spec(7));
    let s = stack();
    let ids = s.load(&repo).await;
    let last = *ids.last().unwrap();
    let paths = every_path(&repo);
    let mut multi_page = 0;
    for path in &paths {
        let rp = RepoPath::parse(path).unwrap();
        for fp in [false, true] {
            let full = full_log(&s, &last, &rp, fp).await;
            for limit in [1usize, 2, 3] {
                let mut got: Vec<ObjectId> = Vec::new();
                let mut token: Option<Vec<u8>> = None;
                let mut pages = 0;
                loop {
                    let page = log_path(&s.engine, &last, &rp, fp, limit, token.as_deref())
                        .await
                        .unwrap();
                    assert!(page.changesets.len() <= limit);
                    got.extend(page.changesets);
                    pages += 1;
                    match page.next_page {
                        Some(t) => token = Some(t),
                        None => break,
                    }
                }
                assert_eq!(got, full, "path {:?} limit {limit}", path);
                if pages > 2 {
                    multi_page += 1;
                }
            }
        }
    }
    assert!(multi_page > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn limit_zero_tokens_and_token_binding() {
    let s = stack();
    let empty = MTree::new();
    let t0 = tree(&[("a/f", "1\n"), ("b", "x\n")]);
    let t1 = tree(&[("a/f", "2\n"), ("b", "x\n")]);
    let t2 = tree(&[("a/f", "2\n"), ("b", "y\n")]);
    let t3 = tree(&[("a/f", "3\n"), ("b", "y\n")]);
    let c0 = s
        .commit_tree(vec![], &empty, &t0, 0, true, Extensions::new())
        .await
        .unwrap();
    let c1 = s
        .commit_tree(vec![c0], &t0, &t1, 1, false, Extensions::new())
        .await
        .unwrap();
    let c2 = s
        .commit_tree(vec![c1], &t1, &t2, 2, false, Extensions::new())
        .await
        .unwrap();
    let c3 = s
        .commit_tree(vec![c2], &t2, &t3, 3, false, Extensions::new())
        .await
        .unwrap();
    let f = p("a/f");
    assert_eq!(full_log(&s, &c3, &f, false).await, vec![c3, c1, c0]);
    assert_eq!(full_log(&s, &c3, &p("a"), false).await, vec![c3, c1, c0]);
    assert_eq!(full_log(&s, &c3, &p("b"), false).await, vec![c2, c0]);
    assert_eq!(
        full_log(&s, &c3, &RepoPath::root(), true).await,
        vec![c3, c2, c1, c0]
    );
    // A path absent at `from` has an empty log.
    let none = log_path(&s.engine, &c3, &p("nope"), false, 10, None)
        .await
        .unwrap();
    assert_eq!(
        none,
        PathLogPage {
            changesets: vec![],
            next_page: None
        }
    );

    // limit 0: empty page, token for the unchanged position.
    let z = log_path(&s.engine, &c3, &f, false, 0, None).await.unwrap();
    assert!(z.changesets.is_empty());
    let tok = z.next_page.unwrap();
    let rest = log_path(&s.engine, &c3, &f, false, 10, Some(&tok))
        .await
        .unwrap();
    assert_eq!(rest.changesets, vec![c3, c1, c0]);
    assert!(rest.next_page.is_none());

    // A token is bound to from, path and first_parent_only.
    let first = log_path(&s.engine, &c3, &f, false, 1, None).await.unwrap();
    let tok = first.next_page.unwrap();
    let bad = |r: scion_core::Result<PathLogPage>| match r {
        Err(e) => e.subcode() == Some(BAD_PAGE_TOKEN),
        Ok(_) => false,
    };
    assert!(bad(log_path(&s.engine, &c3, &f, true, 1, Some(&tok)).await));
    assert!(bad(
        log_path(&s.engine, &c3, &p("b"), false, 1, Some(&tok)).await
    ));
    assert!(bad(log_path(&s.engine, &c2, &f, false, 1, Some(&tok)).await));
    assert!(bad(
        log_path(&s.engine, &c3, &f, false, 1, Some(b"junk")).await
    ));

    // A forged token naming a node of another path is rejected: build the token of `b`'s
    // walk and present it as `a/f`'s by re-encoding with the same frontier.
    let b_first = log_path(&s.engine, &c3, &p("b"), false, 1, None)
        .await
        .unwrap();
    let b_tok = b_first.next_page.unwrap();
    let forged = reencode_with_path(&b_tok, b"a/f");
    let r = log_path(&s.engine, &c3, &f, false, 1, Some(&forged)).await;
    assert!(bad(r));

    // Missing `from`.
    let unknown = ObjectId::new(Kind::Changeset, [3; 32]);
    let r = log_path(&s.engine, &unknown, &f, false, 1, None).await;
    assert!(is_code(&r, ErrorCode::NotFound), "{r:?}");
}

/// Re-encode a log_path token with another path (same layout as the production encoder:
/// `uvarint 1, id from, bytes path, u8 flag, list<id> frontier`).
fn reencode_with_path(token: &[u8], path: &[u8]) -> Vec<u8> {
    let mut d = scion_object::enc::Decoder::new(token);
    let v = d.uvarint().unwrap();
    let from = d.id().unwrap();
    let _old = d.bytes().unwrap();
    let flag = d.u8().unwrap();
    let n = d.list_len().unwrap();
    let frontier: Vec<ObjectId> = (0..n).map(|_| d.id().unwrap()).collect();
    let mut e = scion_object::enc::Encoder::new();
    e.uvarint(v);
    e.id(&from);
    e.bytes(path);
    e.u8(flag);
    e.uvarint(frontier.len() as u64);
    for id in &frontier {
        e.id(id);
    }
    e.finish()
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_keeping_first_parent_version_is_treesame() {
    // c0 f=1; c1 (on c0) f=2; c2 (on c0) g changes; m = merge(c2, c1) keeping f=1 (c2's
    // version): f's log from m is just c0 (TREESAME through the first parent), while
    // merge(c1, c2) with f=2 shows c1 and c0.
    let s = stack();
    let empty = MTree::new();
    let t0 = tree(&[("f", "1\n"), ("g", "a\n")]);
    let t1 = tree(&[("f", "2\n"), ("g", "a\n")]);
    let t2 = tree(&[("f", "1\n"), ("g", "b\n")]);
    let tm = tree(&[("f", "1\n"), ("g", "b\n")]);
    let c0 = s
        .commit_tree(vec![], &empty, &t0, 0, true, Extensions::new())
        .await
        .unwrap();
    let c1 = s
        .commit_tree(vec![c0], &t0, &t1, 1, false, Extensions::new())
        .await
        .unwrap();
    let c2 = s
        .commit_tree(vec![c0], &t0, &t2, 2, false, Extensions::new())
        .await
        .unwrap();
    let m = s
        .commit_tree(vec![c2, c1], &t2, &tm, 3, false, Extensions::new())
        .await
        .unwrap();
    assert_eq!(full_log(&s, &m, &p("f"), false).await, vec![c0]);
    let tm2 = tree(&[("f", "2\n"), ("g", "b\n")]);
    let m2 = s
        .commit_tree(vec![c1, c2], &t1, &tm2, 4, false, Extensions::new())
        .await
        .unwrap();
    assert_eq!(full_log(&s, &m2, &p("f"), false).await, vec![c1, c0]);
    // g at m2: m2's g differs from c1's, so m2 creates a node whose parents are g's nodes
    // in c1 (c0's) and c2.
    let mut want = vec![m2, c2, c0];
    let gens = generations(&s, &want).await;
    want.sort_by(|a, b| gens[b].cmp(&gens[a]).then(b.cmp(a)));
    assert_eq!(full_log(&s, &m2, &p("g"), false).await, want);
    assert_eq!(full_log(&s, &m2, &p("g"), true).await, vec![m2, c0]);

    // all_versions sees the side-branch version of f that m discarded.
    let v = all_versions(&s.engine, &m, &p("f")).await.unwrap();
    assert_eq!(
        v,
        vec![ObjectId::hash_blob(b"1\n"), ObjectId::hash_blob(b"2\n")]
    );
}

async fn check_versions_corpus(s: &Stack, repo: &GenRepo, ids: &[ObjectId]) -> usize {
    let model = model_of(repo);
    let paths = every_path(repo);
    let n = repo.commits.len();
    let mut checked = 0;
    let mut multi = 0;
    for c in [n / 2, n - 1] {
        for path in &paths {
            let rp = RepoPath::parse(path).unwrap();
            let got = all_versions(&s.engine, &ids[c], &rp).await.unwrap();
            let want: Vec<ObjectId> = model
                .all_versions(c, path)
                .iter()
                .map(|content| ObjectId::hash_blob(content))
                .collect();
            assert_eq!(
                got,
                want,
                "commit {c}, path {:?}",
                String::from_utf8_lossy(path)
            );
            if got.len() > 1 {
                multi += 1;
            }
            checked += 1;
        }
    }
    assert!(multi > 0, "some path held several versions");
    checked
}

#[tokio::test(flavor = "multi_thread")]
async fn all_versions_matches_refmodel_on_generated_corpora() {
    let seeds = u64::from(proptest_cases(2).max(1));
    for seed in 0..seeds {
        let repo = generate(&spec(100 + seed));
        let s = stack();
        let ids = s.load(&repo).await;
        assert!(check_versions_corpus(&s, &repo, &ids).await > 0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn all_versions_includes_versions_before_deletion() {
    let s = stack();
    let empty = MTree::new();
    let t0 = tree(&[("d/f", "v1\n"), ("k", "k\n")]);
    let t1 = tree(&[("k", "k\n")]);
    let t2 = tree(&[("d/f", "v2\n"), ("k", "k\n")]);
    let t3 = tree(&[("d/f", "v1\n"), ("k", "k2\n")]);
    let c0 = s
        .commit_tree(vec![], &empty, &t0, 0, true, Extensions::new())
        .await
        .unwrap();
    let c1 = s
        .commit_tree(vec![c0], &t0, &t1, 1, false, Extensions::new())
        .await
        .unwrap();
    let c2 = s
        .commit_tree(vec![c1], &t1, &t2, 2, false, Extensions::new())
        .await
        .unwrap();
    let c3 = s
        .commit_tree(vec![c2], &t2, &t3, 3, false, Extensions::new())
        .await
        .unwrap();
    let v = all_versions(&s.engine, &c3, &p("d/f")).await.unwrap();
    assert_eq!(
        v,
        vec![ObjectId::hash_blob(b"v1\n"), ObjectId::hash_blob(b"v2\n")]
    );
    // The path's history nodes alone (Log) do not reach before the deletion.
    assert_eq!(full_log(&s, &c3, &p("d/f"), false).await, vec![c3, c2]);
    // A directory or a missing path holds no blob.
    assert!(
        all_versions(&s.engine, &c3, &p("d"))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        all_versions(&s.engine, &c3, &p("zz"))
            .await
            .unwrap()
            .is_empty()
    );
    let unknown = ObjectId::new(Kind::Changeset, [5; 32]);
    let r = all_versions(&s.engine, &unknown, &p("d/f")).await;
    assert!(is_code(&r, ErrorCode::NotFound), "{r:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spec-sized (SCION_FULL): default corpora"]
async fn full_log_and_versions_match_refmodel_on_default_corpora() {
    if !scion_core::test_support::full_mode() {
        eprintln!("SKIP: SCION_FULL unset");
        return;
    }
    for seed in 0..2 {
        let repo = generate(&CorpusSpec {
            seed: 900 + seed,
            ..CorpusSpec::default()
        });
        let s = stack();
        let ids = s.load(&repo).await;
        check_log_corpus(&s, &repo, &ids).await;
        check_versions_corpus(&s, &repo, &ids).await;
    }
}
