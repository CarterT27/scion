//! SPEC 19.4 differential tests against the independent reference model (`scion-refmodel`):
//!
//! - `Merge(a, b)` with a unique base matches `merge_trees(base, a, b)` on generated corpora
//!   (`scion_testkit::generate`), and pairs with several best common ancestors (criss-cross)
//!   return AMBIGUOUS_MERGE_BASE;
//! - random three-way scenarios (text edits, mode flips, symlinks, binary content, file/directory
//!   transitions, identical edits) give the same trees and conflict sets for both `Merge(a, b)`
//!   and `Rebase(b, a)` as the model (rebase examines only `b`'s delta paths; an unchanged
//!   result is EMPTY_CHANGE);
//! - no stored object contains a conflict marker afterwards.
//!
//! Default runs use scaled counts (PLAN 3.5); `full_*` variants run spec-sized counts.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod merge_support;

use merge_support::*;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use scion_core::{ErrorCode, ObjectId, test_support};
use scion_history::{History, HistoryConfig};
use scion_refmodel::{MConflict, MDelta, MFile, MPath, MTree, merge_trees};
use scion_testkit::{CorpusSpec, GenRepo, generate};
use std::collections::BTreeSet;

fn max_text() -> u64 {
    HistoryConfig::default().limits.merge_max_text_bytes
}

/// Our conflict list as model conflicts.
fn as_model(err: &scion_core::ScionError) -> Vec<MConflict> {
    report(err)
        .conflicts
        .iter()
        .map(|c| MConflict {
            path: c.path.clone(),
            kind: kind_str(c.kind),
        })
        .collect()
}

/// Load every commit of `repo` (roots in import mode) and return their ids.
async fn load(h: &History, repo: &GenRepo, tag: &str) -> Vec<ObjectId> {
    let mut ids: Vec<ObjectId> = Vec::with_capacity(repo.commits.len());
    let empty = MTree::new();
    for (k, c) in repo.commits.iter().enumerate() {
        let parents: Vec<ObjectId> = c.parents.iter().map(|&i| ids[i]).collect();
        let parent_tree = c.parents.first().map_or(&empty, |&i| &repo.commits[i].tree);
        let id = commit_tree(h, parents, parent_tree, &c.tree, &format!("{tag} {k}")).await;
        ids.push(id);
    }
    ids
}

/// Compare `Merge(i, j)` with the model for every pair of commits (sampled for large repos).
async fn corpus_differential(env: &Env, repo: &GenRepo, tag: &str, max_pairs: usize) -> usize {
    let h = &env.h;
    let ids = load(h, repo, tag).await;
    let n = repo.commits.len();
    let mut compared = 0usize;
    let mut ambiguous = 0usize;
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for i in 0..n {
        for j in 0..n {
            if i != j {
                pairs.push((i, j));
            }
        }
    }
    let stride = (pairs.len() / max_pairs.max(1)).max(1);
    for &(i, j) in pairs.iter().step_by(stride) {
        let bases = repo.merge_bases(i, j);
        let ours = h.merge_bases(&ids[i], &ids[j]).await.unwrap();
        assert_eq!(ours.len(), bases.len(), "{tag}: merge bases of {i},{j}");
        let result = h
            .merge(
                &ids[i],
                &ids[j],
                fields("corpus merge"),
                change_id_for(&[ids[i], ids[j]], "corpus merge"),
                &alice(),
                &scion_history::AllowAll,
            )
            .await;
        if bases.len() > 1 {
            assert_eq!(
                result.unwrap_err().code,
                ErrorCode::AmbiguousMergeBase,
                "{tag}: {i},{j}"
            );
            ambiguous += 1;
            continue;
        }
        let empty = MTree::new();
        let base_tree = bases.first().map_or(&empty, |&b| &repo.commits[b].tree);
        let expected = merge_trees(
            base_tree,
            &repo.commits[i].tree,
            &repo.commits[j].tree,
            None,
            max_text(),
            &BTreeSet::new(),
        );
        match (expected, result) {
            (Ok(tree), Ok(m)) => {
                let got = snapshot(h, &root_of(h, &m).await).await;
                assert_eq!(got, tree, "{tag}: merge {i},{j}");
                let rec = h.changeset(&m).await.unwrap();
                assert_eq!(rec.changeset.parents, vec![ids[i], ids[j]]);
            }
            (Err(conflicts), Err(e)) => {
                assert_eq!(as_model(&e), conflicts, "{tag}: merge {i},{j}");
            }
            (Ok(_), Err(e)) => panic!("{tag}: merge {i},{j}: model clean, engine failed: {e}"),
            (Err(c), Ok(_)) => panic!("{tag}: merge {i},{j}: model conflicts {c:?}, engine clean"),
        }
        compared += 1;
    }
    eprintln!("{tag}: {compared} merges compared, {ambiguous} ambiguous");
    compared
}

fn corpus_spec(seed: u64) -> CorpusSpec {
    CorpusSpec {
        files: 40,
        depth: 3,
        fanout: 3,
        history_len: 24,
        merge_ratio: 0.3,
        octopus_ratio: 0.05,
        binary_ratio: 0.05,
        symlink_ratio: 0.05,
        seed,
    }
}

async fn run_corpora(corpora: u64, max_pairs: usize) {
    for seed in 0..corpora {
        let env = env().await;
        let compared = corpus_differential(
            &env,
            &generate(&corpus_spec(seed)),
            &format!("gen{seed}"),
            max_pairs,
        )
        .await;
        assert!(compared > 0);
        assert_no_markers(&env).await;
    }
}

#[tokio::test]
async fn merge_matches_model_on_generated_corpora() {
    run_corpora(u64::from(test_support::proptest_cases(3)), 120).await;
}

#[tokio::test]
async fn criss_cross_and_multi_root_corpora() {
    let env = env().await;
    let spec = corpus_spec(77);
    let cc = GenRepo::criss_cross(&spec);
    corpus_differential(&env, &cc, "criss-cross", usize::MAX).await;
    let mr = GenRepo::multi_root(&spec, 3);
    corpus_differential(&env, &mr, "multi-root", usize::MAX).await;
    // The canonical heads: the last two commits of the criss-cross shape.
    let n = cc.commits.len();
    assert!(cc.merge_bases(n - 2, n - 1).len() >= 2);
    assert_no_markers(&env).await;
}

#[tokio::test]
#[ignore = "spec-sized (SCION_FULL): many generated corpora"]
async fn full_merge_matches_model_on_generated_corpora() {
    run_corpora(200, 400).await;
}

// ---- random three-way scenarios ------------------------------------------------------------

const NAMES: [&str; 4] = ["a", "b", "c", "d"];

fn random_path(rng: &mut ChaCha8Rng) -> MPath {
    let depth = rng.random_range(1..=3);
    let comps: Vec<&str> = (0..depth)
        .map(|_| NAMES[rng.random_range(0..NAMES.len())])
        .collect();
    comps.join("/").into_bytes()
}

fn random_lines(rng: &mut ChaCha8Rng) -> Vec<Vec<u8>> {
    let n = rng.random_range(0..8);
    (0..n)
        .map(|_| format!("x{}\n", rng.random_range(0..6)).into_bytes())
        .collect()
}

fn join(lines: &[Vec<u8>], unterminated: bool) -> Vec<u8> {
    let mut v: Vec<u8> = lines.concat();
    if unterminated && v.last() == Some(&b'\n') {
        v.pop();
    }
    v
}

fn random_file(rng: &mut ChaCha8Rng) -> MFile {
    let roll = rng.random_range(0..100);
    if roll < 5 {
        return MFile {
            mode: 3,
            content: format!("target{}", rng.random_range(0..3)).into_bytes(),
        };
    }
    let mut content = join(&random_lines(rng), rng.random_bool(0.1));
    if roll < 10 {
        content.extend_from_slice(b"\0bin");
    }
    MFile {
        mode: if rng.random_bool(0.15) { 2 } else { 1 },
        content,
    }
}

/// Insert `f` at `p`, removing files at ancestors of `p` and below `p` (a valid file/directory
/// transition).
fn insert(t: &mut MTree, p: MPath, f: MFile) {
    let mut prefix = p.clone();
    prefix.push(b'/');
    t.retain(|k, _| {
        let below = k.starts_with(&prefix);
        let ancestor = p.len() > k.len() && p.starts_with(k) && p[k.len()] == b'/';
        !(below || ancestor)
    });
    t.insert(p, f);
}

/// Edit one line of a text file (replace, insert or delete) or flip its mode.
fn edit(rng: &mut ChaCha8Rng, f: &MFile) -> MFile {
    if f.mode == 3 {
        return MFile {
            mode: 3,
            content: format!("target{}", rng.random_range(3..9)).into_bytes(),
        };
    }
    if rng.random_bool(0.15) {
        return MFile {
            mode: if f.mode == 1 { 2 } else { 1 },
            content: f.content.clone(),
        };
    }
    let unterminated = !f.content.is_empty() && f.content.last() != Some(&b'\n');
    let mut lines: Vec<Vec<u8>> = f
        .content
        .split_inclusive(|b| *b == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    if let Some(last) = lines.last_mut()
        && last.last() != Some(&b'\n')
    {
        last.push(b'\n');
    }
    let new_line = format!("y{}\n", rng.random_range(0..4)).into_bytes();
    match (rng.random_range(0..3), lines.len()) {
        (_, 0) | (0, _) => {
            let at = rng.random_range(0..=lines.len());
            lines.insert(at, new_line);
        }
        (1, n) => {
            let at = rng.random_range(0..n);
            lines[at] = new_line;
        }
        (_, n) => {
            lines.remove(rng.random_range(0..n));
        }
    }
    MFile {
        mode: f.mode,
        content: join(&lines, unterminated),
    }
}

fn mutate(rng: &mut ChaCha8Rng, base: &MTree) -> MTree {
    let mut t = base.clone();
    let ops = rng.random_range(1..=4);
    for _ in 0..ops {
        let keys: Vec<MPath> = t.keys().cloned().collect();
        match rng.random_range(0..10) {
            0..=4 if !keys.is_empty() => {
                let k = keys[rng.random_range(0..keys.len())].clone();
                let f = edit(rng, &t[&k]);
                t.insert(k, f);
            }
            5..=6 if !keys.is_empty() => {
                let k = &keys[rng.random_range(0..keys.len())];
                t.remove(k);
            }
            _ => {
                let p = random_path(rng);
                let f = random_file(rng);
                insert(&mut t, p, f);
            }
        }
    }
    t
}

/// A random scenario `(base, ours, theirs)` with both sides different from the base.
fn scenario(rng: &mut ChaCha8Rng) -> (MTree, MTree, MTree) {
    loop {
        let mut base = MTree::new();
        for _ in 0..rng.random_range(2..10) {
            let p = random_path(rng);
            let f = random_file(rng);
            insert(&mut base, p, f);
        }
        let ours = mutate(rng, &base);
        let mut theirs = mutate(rng, &base);
        // Sometimes repeat one of ours' changes on theirs (identical edits, row 3).
        if rng.random_bool(0.25) {
            let changed: Vec<MDelta> = scion_refmodel::file_diff(&base, &ours);
            if !changed.is_empty() {
                match &changed[rng.random_range(0..changed.len())] {
                    MDelta::Put(p, f) => insert(&mut theirs, p.clone(), f.clone()),
                    MDelta::Delete(p) => {
                        theirs.remove(p);
                    }
                }
            }
        }
        if ours != base && theirs != base {
            return (base, ours, theirs);
        }
    }
}

/// Outcome counts of the random scenarios (coverage of the SPEC 8.3 rows).
#[derive(Default, Debug)]
struct Stats {
    clean: usize,
    empty: usize,
    kinds: std::collections::BTreeMap<&'static str, usize>,
}

/// One random scenario: `P = root + base`, `a = P + ours`, `b = P + theirs`; with `redact_one`,
/// one random content of the scenario is quarantined first (SPEC 13.6, 19.13) and the model
/// receives it as redacted. Trees are compared by `(mode, id)` so redacted entries need no
/// read.
async fn three_way_case(
    env: &Env,
    rng: &mut ChaCha8Rng,
    case: usize,
    redact_one: bool,
    stats: &mut Stats,
) {
    let h = &env.h;
    let (base, ours, theirs) = scenario(rng);
    let empty = MTree::new();
    let p = commit_tree(h, vec![env.root], &empty, &base, &format!("base {case}")).await;
    let a = commit_tree(h, vec![p], &base, &ours, &format!("ours {case}")).await;
    let b = commit_tree(h, vec![p], &base, &theirs, &format!("theirs {case}")).await;

    let mut redacted: BTreeSet<Vec<u8>> = BTreeSet::new();
    if redact_one {
        let contents: Vec<&Vec<u8>> = base
            .values()
            .chain(ours.values())
            .chain(theirs.values())
            .map(|f| &f.content)
            .collect();
        let c = contents[rng.random_range(0..contents.len())].clone();
        redact(env, &[ObjectId::hash_blob(&c)]).await;
        redacted.insert(c);
    }
    let ctx = || format!("case {case}: {base:?} {ours:?} {theirs:?} redacted {redacted:?}");

    // Merge(a, b) against the model over both FileDiffs.
    let expected = merge_trees(&base, &ours, &theirs, None, max_text(), &redacted);
    let got = h
        .merge(
            &a,
            &b,
            fields("m"),
            change_id_for(&[a, b], "m"),
            &alice(),
            &scion_history::AllowAll,
        )
        .await;
    match (&expected, got) {
        (Ok(tree), Ok(m)) => {
            stats.clean += 1;
            let got = snapshot_ids(h, &root_of(h, &m).await).await;
            assert_eq!(got, model_ids(tree), "{}", ctx());
        }
        (Err(c), Err(e)) => {
            for k in c {
                *stats.kinds.entry(k.kind).or_default() += 1;
            }
            assert_eq!(&as_model(&e), c, "{}", ctx());
        }
        (x, y) => panic!("merge: model {x:?} vs engine {y:?}; {}", ctx()),
    }

    // Rebase(b, a): only b's delta paths are examined.
    let paths: BTreeSet<MPath> = scion_refmodel::file_diff(&base, &theirs)
        .into_iter()
        .map(|d| match d {
            MDelta::Put(p, _) | MDelta::Delete(p) => p,
        })
        .collect();
    let expected = merge_trees(&base, &ours, &theirs, Some(&paths), max_text(), &redacted);
    let got = h.rebase(&b, &a, &alice()).await;
    match (&expected, got) {
        (Ok(tree), Err(e)) if *tree == ours => {
            stats.empty += 1;
            assert_eq!(e.code, ErrorCode::EmptyChange, "{}", ctx());
        }
        (Ok(tree), Ok(r)) => {
            let got = snapshot_ids(h, &root_of(h, &r).await).await;
            assert_eq!(got, model_ids(tree), "{}", ctx());
            let rec = h.changeset(&r).await.unwrap();
            assert_eq!(rec.changeset.parents, vec![a]);
        }
        (Err(c), Err(e)) => assert_eq!(&as_model(&e), c, "rebase: {}", ctx()),
        (x, y) => panic!("rebase: model {x:?} vs engine {y:?}; {}", ctx()),
    }
}

async fn run_three_way(cases: usize, seed: u64) {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut env = env().await;
    let mut stats = Stats::default();
    for case in 0..cases {
        if case % 200 == 199 {
            assert_no_markers(&env).await;
            env = merge_support::env().await;
        }
        three_way_case(&env, &mut rng, case, false, &mut stats).await;
    }
    assert_no_markers(&env).await;
    eprintln!("three-way outcomes over {cases} cases: {stats:?}");
    if cases >= 300 {
        assert!(stats.clean > 0 && stats.empty > 0, "{stats:?}");
        for kind in ["add_add", "modify_delete", "content", "file_directory"] {
            assert!(
                stats.kinds.contains_key(kind),
                "no {kind} conflict: {stats:?}"
            );
        }
    }
}

/// Every case runs in a fresh repository, because a redacted content may not be re-uploaded
/// by a later scenario (SPEC 13.6).
async fn run_three_way_redacted(cases: usize, seed: u64) {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut stats = Stats::default();
    for case in 0..cases {
        let env = env().await;
        three_way_case(&env, &mut rng, case, true, &mut stats).await;
        assert_no_markers(&env).await;
    }
    eprintln!("redacted three-way outcomes over {cases} cases: {stats:?}");
    if cases >= 200 {
        assert!(stats.kinds.contains_key("redacted"), "{stats:?}");
    }
}

#[tokio::test]
async fn random_three_way_matches_model() {
    run_three_way(test_support::proptest_cases(300) as usize, 0x5c10).await;
}

#[tokio::test]
async fn random_three_way_with_redacted_content_matches_model() {
    run_three_way_redacted(test_support::proptest_cases(200) as usize, 0x5c12).await;
}

#[tokio::test]
#[ignore = "spec-sized (SCION_FULL): 10^6 three-way scenarios"]
async fn full_random_three_way_matches_model() {
    let cases = if test_support::full_mode() {
        1_000_000
    } else {
        100_000
    };
    run_three_way(cases, 0x5c11).await;
    run_three_way_redacted(cases / 10, 0x5c13).await;
}
