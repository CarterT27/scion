//! SPEC 19.6 `blame`: `scion_derived::blame::blame` equals `scion_refmodel::MRepo::blame` for
//! every file at several changesets of generated corpora with merges (end to end through
//! `History::create_changeset` with `DerivedEngine` as the EagerHook), plus hand examples and
//! the NOT_FOUND cases (P24). Spec-sized corpora run in `full_*` (SCION_FULL).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod history_support;

use history_support::*;
use scion_core::{ErrorCode, ObjectId, test_support::proptest_cases};
use scion_derived::blame::{self, BlameRange};
use scion_object::Extensions;
use scion_refmodel::MTree;
use scion_testkit::{CorpusSpec, GenRepo, generate};

/// The reference model's blame of `path` at commit `c`, with origins mapped to changeset ids.
fn model_blame(
    model: &scion_refmodel::MRepo,
    ids: &[ObjectId],
    c: usize,
    path: &[u8],
) -> Vec<BlameRange> {
    model
        .blame(c, path)
        .into_iter()
        .map(|(start, count, origin, ostart)| BlameRange {
            start_line: start,
            line_count: count,
            origin_changeset: ids[origin],
            origin_start_line: ostart,
        })
        .collect()
}

/// Compare blame with the model for every file at the commits selected by `pick`; returns the
/// number of (commit, file) pairs checked.
async fn check_corpus(s: &Stack, repo: &GenRepo, ids: &[ObjectId], pick: &[usize]) -> usize {
    let model = model_of(repo);
    let mut checked = 0;
    for &c in pick {
        for path in repo.commits[c].tree.keys() {
            let rp = scion_object::RepoPath::parse(path).unwrap();
            let got = blame::blame(&s.engine, &ids[c], &rp, None).await.unwrap();
            let want = model_blame(&model, ids, c, path);
            assert_eq!(
                got,
                want,
                "commit {c}, path {:?}",
                String::from_utf8_lossy(path)
            );
            checked += 1;
        }
    }
    checked
}

/// Several commits of `repo`: every `step`-th, every merge, and the last.
fn picks(repo: &GenRepo, step: usize) -> Vec<usize> {
    let n = repo.commits.len();
    let mut out: Vec<usize> = (0..n)
        .filter(|&i| i % step == 0 || repo.commits[i].parents.len() > 1 || i + 1 == n)
        .collect();
    out.dedup();
    out
}

fn spec(seed: u64) -> CorpusSpec {
    CorpusSpec {
        files: 40,
        depth: 3,
        fanout: 3,
        history_len: 36,
        merge_ratio: 0.25,
        octopus_ratio: 0.08,
        seed,
        ..CorpusSpec::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn blame_matches_refmodel_on_generated_corpora() {
    // SCION_PROPTEST_CASES scales the number of seeds (default 3).
    let seeds = u64::from(proptest_cases(3).max(1));
    let mut total = 0;
    let mut merges = 0;
    for seed in 0..seeds {
        let repo = generate(&spec(seed));
        merges += repo.commits.iter().filter(|c| c.parents.len() > 1).count();
        let s = stack();
        let ids = s.load(&repo).await;
        total += check_corpus(&s, &repo, &ids, &picks(&repo, 5)).await;
    }
    assert!(total > 0);
    assert!(merges > 0, "the corpora contain merges");
}

#[tokio::test(flavor = "multi_thread")]
async fn blame_is_stable_once_derived() {
    // A second query is served from the stored value and is byte-identical; a fresh engine
    // over the same storage reads the same stored value.
    let repo = generate(&spec(42));
    let s = stack();
    let ids = s.load(&repo).await;
    let last = repo.commits.len() - 1;
    for path in repo.commits[last].tree.keys().take(10) {
        let rp = scion_object::RepoPath::parse(path).unwrap();
        let a = blame::blame(&s.engine, &ids[last], &rp, None)
            .await
            .unwrap();
        let b = blame::blame(&s.engine, &ids[last], &rp, None)
            .await
            .unwrap();
        assert_eq!(a, b);
        let (node, _) = scion_derived::history::node_for_path(s.engine.ctx(), &ids[last], &rp)
            .await
            .unwrap()
            .unwrap();
        let stored = s
            .engine
            .get_stored(blame::NAME, Some(1), node.as_bytes())
            .await
            .unwrap()
            .expect("blame value written back");
        assert_eq!(blame::decode_ranges(&stored).unwrap(), a);
        assert_eq!(blame::encode_ranges(&a), stored);
    }
}

fn t(entries: &[(&str, &str)]) -> MTree {
    tree(entries)
}

#[tokio::test(flavor = "multi_thread")]
async fn hand_example_with_a_merge() {
    // c0: a b c; c1 (on c0): a B c; c2 (on c0): a b c d; c3 = merge(c1, c2): a B c d e.
    let s = stack();
    let empty = MTree::new();
    let t0 = t(&[("f", "a\nb\nc\n")]);
    let t1 = t(&[("f", "a\nB\nc\n")]);
    let t2 = t(&[("f", "a\nb\nc\nd\n")]);
    let t3 = t(&[("f", "a\nB\nc\nd\ne\n")]);
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
    let c3 = s
        .commit_tree(vec![c1, c2], &t1, &t3, 3, false, Extensions::new())
        .await
        .unwrap();
    let got = blame::blame(&s.engine, &c3, &p("f"), None).await.unwrap();
    let r = |start, count, origin, ostart| BlameRange {
        start_line: start,
        line_count: count,
        origin_changeset: origin,
        origin_start_line: ostart,
    };
    assert_eq!(
        got,
        vec![
            r(0, 1, c0, 0),
            r(1, 1, c1, 1),
            r(2, 1, c0, 2),
            r(3, 1, c2, 3),
            r(4, 1, c3, 4),
        ]
    );
    // The same through the model.
    let mut m = scion_refmodel::MRepo::new();
    m.commit(&[], t0.clone());
    m.commit(&[0], t1.clone());
    m.commit(&[0], t2.clone());
    m.commit(&[1, 2], t3.clone());
    assert_eq!(got, model_blame(&m, &[c0, c1, c2, c3], 3, b"f"));

    // An empty file has no ranges; a file without a trailing newline counts its last line.
    let t4 = t(&[("f", "a\nB\nc\nd\ne\n"), ("g", ""), ("h", "x\ny")]);
    let c4 = s
        .commit_tree(vec![c3], &t3, &t4, 4, false, Extensions::new())
        .await
        .unwrap();
    assert!(
        blame::blame(&s.engine, &c4, &p("g"), None)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        blame::blame(&s.engine, &c4, &p("h"), None).await.unwrap(),
        vec![r(0, 2, c4, 0)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_and_non_file_paths_are_not_found() {
    let s = stack();
    let empty = MTree::new();
    let t0 = t(&[("dir/f", "a\n")]);
    let c0 = s
        .commit_tree(vec![], &empty, &t0, 0, true, Extensions::new())
        .await
        .unwrap();
    for path in ["nope", "dir", "dir/f/x", "dir/g"] {
        let r = blame::blame(&s.engine, &c0, &p(path), None).await;
        assert!(is_code(&r, ErrorCode::NotFound), "{path}: {r:?}");
    }
    let r = blame::blame(&s.engine, &c0, &scion_object::RepoPath::root(), None).await;
    assert!(is_code(&r, ErrorCode::NotFound), "root: {r:?}");
    let unknown = ObjectId::new(scion_core::Kind::Changeset, [7; 32]);
    let r = blame::blame(&s.engine, &unknown, &p("dir/f"), None).await;
    assert!(is_code(&r, ErrorCode::NotFound), "{r:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn file_replacing_a_directory_starts_fresh() {
    // `x` is a directory in c1 and a file in c2 (its node's parent is a directory node, which
    // attributes nothing); `x` again a file in c3 keeps c2's lines.
    let s = stack();
    let empty = MTree::new();
    let t0 = t(&[("keep", "k\n")]);
    let t1 = t(&[("keep", "k\n"), ("x/y", "same\n")]);
    let t2 = t(&[("keep", "k\n"), ("x", "same\nnew\n")]);
    let t3 = t(&[("keep", "k\n"), ("x", "same\nnew\nmore\n")]);
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
    let mut m = scion_refmodel::MRepo::new();
    for (parents, tr) in [
        (vec![], &t0),
        (vec![0], &t1),
        (vec![1], &t2),
        (vec![2], &t3),
    ] {
        m.commit(&parents, tr.clone());
    }
    let ids = [c0, c1, c2, c3];
    let got = blame::blame(&s.engine, &c3, &p("x"), None).await.unwrap();
    assert_eq!(got, model_blame(&m, &ids, 3, b"x"));
    assert_eq!(got.first().unwrap().origin_changeset, c2);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spec-sized (SCION_FULL): default corpora, every commit"]
async fn full_blame_matches_refmodel_on_default_corpora() {
    if !scion_core::test_support::full_mode() {
        eprintln!("SKIP: SCION_FULL unset");
        return;
    }
    for seed in 0..4 {
        let repo = generate(&CorpusSpec {
            seed: 500 + seed,
            ..CorpusSpec::default()
        });
        let s = stack();
        let ids = s.load(&repo).await;
        let all: Vec<usize> = (0..repo.commits.len()).collect();
        check_corpus(&s, &repo, &ids, &all).await;
    }
}
