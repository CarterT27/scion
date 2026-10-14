//! SPEC 19.2 property: for random trees `T1` and random valid deltas, `apply_delta` equals the
//! independent reference model (flattened, and by root id) and `FileDiff(T1, result) == delta`.
//! A second property feeds arbitrary (often invalid) deltas and requires the same outcome as the
//! reference model, including the failure subcode.
//!
//! Default runs use `proptest_cases(2_000)`; the spec-sized 10^5 run is `full_` and ignored.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod create_support;

use create_support::*;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use scion_core::test_support::proptest_cases;
use scion_core::{Kind, Mode, ObjectId};
use scion_history::{History, HistoryConfig};
use scion_object::{DeltaEntry, DeltaOp, RepoPath};
use scion_refmodel::{MDelta, MFile, MTree};
use scion_testkit::encode_tree;
use std::cell::Cell;
use std::collections::BTreeMap;

fn name() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec!["a", "b", "c", "a.b"])
}

fn path() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(name(), 1..=3).prop_map(|c| c.join("/").into_bytes())
}

fn file() -> impl Strategy<Value = MFile> {
    (1u8..=3, prop::sample::select(vec!["x", "y", "z", ""])).prop_map(|(mode, c)| MFile {
        mode,
        content: c.as_bytes().to_vec(),
    })
}

/// `a` is a strict directory prefix of `b`.
fn is_under(a: &[u8], b: &[u8]) -> bool {
    b.len() > a.len() && b.starts_with(a) && b[a.len()] == b'/'
}

/// Insert `p` into `t`, first removing every entry that would make `p` both a file and a
/// directory.
fn insert_clean(t: &mut MTree, p: Vec<u8>, f: MFile) {
    t.retain(|k, _| !is_under(k, &p) && !is_under(&p, k));
    t.insert(p, f);
}

fn tree() -> impl Strategy<Value = MTree> {
    prop::collection::vec((path(), file()), 0..14).prop_map(|entries| {
        let mut t = MTree::new();
        for (p, f) in entries {
            insert_clean(&mut t, p, f);
        }
        t
    })
}

#[derive(Clone, Debug)]
enum Mutation {
    Delete(usize),
    Modify(usize, MFile),
    Add(Vec<u8>, MFile),
}

fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        any::<usize>().prop_map(Mutation::Delete),
        (any::<usize>(), file()).prop_map(|(i, f)| Mutation::Modify(i, f)),
        (path(), file()).prop_map(|(p, f)| Mutation::Add(p, f)),
    ]
}

fn mutate(t1: &MTree, muts: &[Mutation]) -> MTree {
    let mut t2 = t1.clone();
    for m in muts {
        let keys: Vec<Vec<u8>> = t2.keys().cloned().collect();
        match m {
            Mutation::Delete(i) if !keys.is_empty() => {
                t2.remove(&keys[i % keys.len()]);
            }
            Mutation::Modify(i, f) if !keys.is_empty() => {
                t2.insert(keys[i % keys.len()].clone(), f.clone());
            }
            Mutation::Add(p, f) => insert_clean(&mut t2, p.clone(), f.clone()),
            _ => {}
        }
    }
    t2
}

fn to_entry(d: &MDelta) -> DeltaEntry {
    match d {
        MDelta::Put(p, f) => DeltaEntry {
            path: RepoPath::parse(p).unwrap(),
            op: DeltaOp::Put {
                mode: Mode::from_u8(f.mode).unwrap(),
                id: ObjectId::hash_blob(&f.content),
                size: f.content.len() as u64,
            },
        },
        MDelta::Delete(p) => DeltaEntry {
            path: RepoPath::parse(p).unwrap(),
            op: DeltaOp::Delete,
        },
    }
}

fn flat_model(t: &MTree) -> BTreeMap<Vec<u8>, (Mode, ObjectId, u64)> {
    t.iter()
        .map(|(p, f)| {
            (
                p.clone(),
                (
                    Mode::from_u8(f.mode).unwrap(),
                    ObjectId::hash_blob(&f.content),
                    f.content.len() as u64,
                ),
            )
        })
        .collect()
}

async fn flatten(h: &History, root: &ObjectId) -> BTreeMap<Vec<u8>, (Mode, ObjectId, u64)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(RepoPath::root(), *root)];
    while let Some((prefix, id)) = stack.pop() {
        let t = h.tree(&id).await.unwrap();
        for e in &t.entries {
            let p = prefix.join(&e.name).unwrap();
            if e.mode == Mode::Tree {
                stack.push((p, e.id));
            } else {
                out.insert(p.as_bytes().to_vec(), (e.mode, e.id, e.size));
            }
        }
    }
    out
}

async fn store_model(h: &History, t: &MTree) -> ObjectId {
    let (root, objects) = encode_tree(t);
    for (_, kind, bytes) in objects {
        if kind == Kind::Tree {
            h.objects().put_encoded(kind, bytes).await.unwrap();
        }
    }
    root
}

async fn valid_case(h: &History, t1: MTree, muts: Vec<Mutation>) -> Result<(), TestCaseError> {
    let t2 = mutate(&t1, &muts);
    let mdelta = scion_refmodel::file_diff(&t1, &t2);
    prop_assert_eq!(scion_refmodel::apply_delta(&t1, &mdelta), Ok(t2.clone()));
    let delta: Vec<DeltaEntry> = mdelta.iter().map(to_entry).collect();
    let t1_id = store_model(h, &t1).await;

    let applied = h.apply_delta(&t1_id, &delta).await.unwrap();
    prop_assert_eq!(applied.root, encode_tree(&t2).0);
    for (_, id, bytes) in &applied.new_trees {
        prop_assert_eq!(ObjectId::hash_encoded(Kind::Tree, bytes), *id);
        h.objects()
            .put_encoded(Kind::Tree, bytes.clone())
            .await
            .unwrap();
    }
    prop_assert_eq!(flatten(h, &applied.root).await, flat_model(&t2));

    let diff = h
        .file_diff(Some(&t1_id), &applied.root, None)
        .await
        .unwrap();
    let as_delta: Vec<DeltaEntry> = diff
        .into_iter()
        .map(|c| DeltaEntry {
            path: c.path,
            op: match c.new {
                Some(n) => DeltaOp::Put {
                    mode: n.mode,
                    id: n.id,
                    size: n.size,
                },
                None => DeltaOp::Delete,
            },
        })
        .collect();
    prop_assert_eq!(as_delta, delta);
    Ok(())
}

async fn arbitrary_case(
    h: &History,
    tally: &Tally,
    t1: MTree,
    ops: Vec<(Vec<u8>, Option<MFile>)>,
) -> Result<(), TestCaseError> {
    let mut by_path: BTreeMap<Vec<u8>, Option<MFile>> = BTreeMap::new();
    for (p, f) in ops {
        by_path.insert(p, f);
    }
    let mdelta: Vec<MDelta> = by_path
        .into_iter()
        .map(|(p, f)| match f {
            Some(f) => MDelta::Put(p, f),
            None => MDelta::Delete(p),
        })
        .collect();
    let delta: Vec<DeltaEntry> = mdelta.iter().map(to_entry).collect();
    let t1_id = store_model(h, &t1).await;
    let model = scion_refmodel::apply_delta(&t1, &mdelta);
    let prod = h.apply_delta(&t1_id, &delta).await;
    match (model, prod) {
        (Ok(t2), Ok(applied)) => {
            tally.arbitrary_ok.set(tally.arbitrary_ok.get() + 1);
            prop_assert_eq!(applied.root, encode_tree(&t2).0)
        }
        (Err(code), Err(e)) => {
            tally.arbitrary_err.set(tally.arbitrary_err.get() + 1);
            prop_assert_eq!(Some(code), e.subcode())
        }
        (m, p) => {
            return Err(TestCaseError::fail(format!(
                "model {m:?} vs production {:?}",
                p.map(|a| a.root)
            )));
        }
    }
    Ok(())
}

/// Outcome tallies, so the run proves both branches are exercised.
#[derive(Default)]
struct Tally {
    valid: Cell<u32>,
    arbitrary_ok: Cell<u32>,
    arbitrary_err: Cell<u32>,
}

fn run(cases: u32) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (h, _mem, _clock) = history_with(HistoryConfig::default());
    let tally = Tally::default();
    let config = Config {
        cases,
        failure_persistence: None,
        ..Config::default()
    };

    let mut runner = TestRunner::new(config.clone());
    runner
        .run(
            &(tree(), prop::collection::vec(mutation(), 0..8)),
            |(t1, muts)| {
                tally.valid.set(tally.valid.get() + 1);
                rt.block_on(valid_case(&h, t1, muts))
            },
        )
        .unwrap();

    let mut runner = TestRunner::new(config);
    let op = (path(), prop::option::of(file()));
    runner
        .run(&(tree(), prop::collection::vec(op, 0..6)), |(t1, ops)| {
            rt.block_on(arbitrary_case(&h, &tally, t1, ops))
        })
        .unwrap();
    assert!(tally.valid.get() >= cases);
    assert!(tally.arbitrary_ok.get() + tally.arbitrary_err.get() >= cases);
    assert!(tally.arbitrary_ok.get() > 0 && tally.arbitrary_err.get() > 0);
}

#[test]
fn apply_delta_matches_reference_model() {
    run(proptest_cases(2_000));
}

#[test]
#[ignore = "spec-sized (SCION_FULL): 10^5 cases"]
fn full_apply_delta_matches_reference_model() {
    run(proptest_cases(100_000));
}
