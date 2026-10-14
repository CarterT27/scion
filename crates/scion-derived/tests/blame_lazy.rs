//! SPEC 19.6 / 13.5 `blame` laziness (P24):
//!
//! - a 10^4-node single-file history derives iteratively from nothing (on a 2 MiB thread) and
//!   writes back every intermediate value (`full_*`: 10^5, SCION_FULL);
//! - a version bump (`BlameType::with_version(2)`) re-derives on query while version-1 values
//!   stay readable through `DerivedEngine::get`;
//! - blame over history whose content was redacted is deterministic (redacted reads as empty)
//!   and equals the reference model's `blame_redacted`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod history_support;

use bytes::Bytes;
use history_support::*;
use scion_core::{Kind, ManualClock, ObjectId};
use scion_derived::blame::{self, BlameRange, BlameType};
use scion_derived::history::NODE_KIND;
use scion_derived::{DeriveConfig, DerivedEngine, Registry};
use scion_object::{Canonical, Extensions, HistoryNode, NodeKind};
use scion_refmodel::MTree;
use scion_store::{MemOptions, ObjectRepo, ObjectRepoConfig, RedactTxn, Storage};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

fn synthetic_changeset(i: u64) -> ObjectId {
    let mut d = [0u8; 32];
    d[..8].copy_from_slice(&i.to_be_bytes());
    d[8] = 0xB1;
    ObjectId::new(Kind::Changeset, d)
}

fn chain_content(i: u64) -> Vec<u8> {
    format!("head\nv{i}\ntail\n").into_bytes()
}

/// Store a single-file history of `n` nodes directly (blob + node per step; node `i` has
/// parent node `i - 1`), returning the node ids.
async fn build_chain(objects: &ObjectRepo, n: u64) -> Vec<ObjectId> {
    let mut nodes: Vec<ObjectId> = Vec::with_capacity(n as usize);
    for i in 0..n {
        let (blob, _) = objects
            .put_blob(Bytes::from(chain_content(i)))
            .await
            .unwrap();
        let node = HistoryNode {
            kind: NodeKind::File,
            content: blob,
            changeset: synthetic_changeset(i),
            parents: nodes.last().copied().into_iter().collect(),
            children: Vec::new(),
            extensions: Extensions::new(),
        };
        let (id, _) = objects
            .put_encoded(NODE_KIND, Bytes::from(node.encode()))
            .await
            .unwrap();
        nodes.push(id);
    }
    nodes
}

fn expected_chain_blame(i: u64) -> Vec<BlameRange> {
    let c0 = synthetic_changeset(0);
    let r = |start, origin, ostart| BlameRange {
        start_line: start,
        line_count: 1,
        origin_changeset: origin,
        origin_start_line: ostart,
    };
    if i == 0 {
        return vec![BlameRange {
            start_line: 0,
            line_count: 3,
            origin_changeset: c0,
            origin_start_line: 0,
        }];
    }
    vec![r(0, c0, 0), r(1, synthetic_changeset(i), 1), r(2, c0, 2)]
}

fn chain_on_thread(n: u64) {
    let handle = std::thread::Builder::new()
        .name("blame-chain".to_owned())
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let (storage, _mem) = Storage::in_memory(MemOptions::default());
                let clock = ManualClock::new(SystemTime::UNIX_EPOCH);
                let objects = ObjectRepo::new(storage, ObjectRepoConfig::default());
                let nodes = build_chain(&objects, n).await;
                let engine = DerivedEngine::new(
                    objects,
                    Registry::v1_default(),
                    DeriveConfig::default(),
                    clock,
                );
                let last = *nodes.last().unwrap();
                let v = engine
                    .get(blame::NAME, None, last.as_bytes())
                    .await
                    .unwrap();
                assert_eq!(
                    blame::decode_ranges(&v).unwrap(),
                    expected_chain_blame(n - 1)
                );
                // Every intermediate value was written back.
                for i in [0, n / 3, n / 2, n - 2] {
                    let stored = engine
                        .get_stored(blame::NAME, Some(1), nodes[i as usize].as_bytes())
                        .await
                        .unwrap()
                        .unwrap_or_else(|| panic!("node {i} written back"));
                    assert_eq!(
                        blame::decode_ranges(&stored).unwrap(),
                        expected_chain_blame(i)
                    );
                }
            });
        })
        .unwrap();
    handle.join().expect("blame chain thread");
}

#[test]
fn lazy_blame_over_a_long_single_file_history_is_iterative() {
    chain_on_thread(10_000);
}

#[test]
#[ignore = "spec-sized (SCION_FULL): 10^5-node chain"]
fn full_lazy_blame_chain_1e5() {
    if !scion_core::test_support::full_mode() {
        eprintln!("SKIP: SCION_FULL unset");
        return;
    }
    chain_on_thread(100_000);
}

/// A small history of `f` in `s`: returns the changesets and the model.
async fn small_history(s: &Stack) -> (Vec<ObjectId>, scion_refmodel::MRepo, Vec<MTree>) {
    let trees = vec![
        tree(&[("f", "a\nb\nc\n"), ("g", "1\n")]),
        tree(&[("f", "a\nX\nc\n"), ("g", "1\n")]),
        tree(&[("f", "a\nX\nc\nd\n"), ("g", "1\n2\n")]),
        tree(&[("f", "a\nX\nc\nd\ne\n"), ("g", "1\n2\n")]),
    ];
    let empty = MTree::new();
    let mut ids: Vec<ObjectId> = Vec::new();
    let mut m = scion_refmodel::MRepo::new();
    for (i, t) in trees.iter().enumerate() {
        let parents: Vec<ObjectId> = ids.last().copied().into_iter().collect();
        let t1 = if i == 0 { &empty } else { &trees[i - 1] };
        let id = s
            .commit_tree(parents, t1, t, i as u64, i == 0, Extensions::new())
            .await
            .unwrap();
        let mp: Vec<usize> = if i == 0 { vec![] } else { vec![i - 1] };
        m.commit(&mp, t.clone());
        ids.push(id);
    }
    (ids, m, trees)
}

fn model_ranges(raw: Vec<(u32, u32, usize, u32)>, ids: &[ObjectId]) -> Vec<BlameRange> {
    raw.into_iter()
        .map(|(start, count, origin, ostart)| BlameRange {
            start_line: start,
            line_count: count,
            origin_changeset: ids[origin],
            origin_start_line: ostart,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn version_bump_rederives_and_old_values_stay_readable() {
    let s = stack();
    let (ids, m, _) = small_history(&s).await;
    let last = *ids.last().unwrap();
    let v1 = blame::blame(&s.engine, &last, &p("f"), None).await.unwrap();
    assert_eq!(v1, model_ranges(m.blame(3, b"f"), &ids));
    let (node, _) = scion_derived::history::node_for_path(s.engine.ctx(), &last, &p("f"))
        .await
        .unwrap()
        .unwrap();
    let v1_bytes = s
        .engine
        .get_stored(blame::NAME, Some(1), node.as_bytes())
        .await
        .unwrap()
        .unwrap();

    // A new engine over the same objects with blame v2 registered next to v1.
    let mut reg = Registry::v1_default();
    reg.register_lazy(Arc::new(BlameType::with_version(2)));
    let e2 = DerivedEngine::new(
        s.h.objects().clone(),
        reg,
        DeriveConfig::default(),
        s.clock.clone(),
    );
    assert_eq!(e2.registry().latest_version(blame::NAME), Some(2));
    assert!(
        e2.get_stored(blame::NAME, Some(2), node.as_bytes())
            .await
            .unwrap()
            .is_none()
    );
    // The unchanged query path (version None) now derives v2.
    let v2 = blame::blame(&e2, &last, &p("f"), None).await.unwrap();
    assert_eq!(v2, v1);
    assert!(
        e2.get_stored(blame::NAME, Some(2), node.as_bytes())
            .await
            .unwrap()
            .is_some(),
        "v2 re-derived and written back"
    );
    // v1 stays readable through the generic path.
    assert_eq!(
        e2.get(blame::NAME, Some(1), node.as_bytes()).await.unwrap(),
        v1_bytes
    );
    assert_eq!(
        blame::blame(&e2, &last, &p("f"), Some(1)).await.unwrap(),
        v1
    );

    // An engine where only v2 is registered still serves stored v1 values.
    let mut only2 = Registry::empty();
    only2.register_eager(Arc::new(scion_derived::history::HistoryType));
    only2.register_lazy(Arc::new(BlameType::with_version(2)));
    let e3 = DerivedEngine::new(
        s.h.objects().clone(),
        only2,
        DeriveConfig::default(),
        s.clock.clone(),
    );
    assert_eq!(
        e3.get(blame::NAME, Some(1), node.as_bytes()).await.unwrap(),
        v1_bytes
    );
}

async fn redact(s: &Stack, ids: &[ObjectId]) {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    s.h.objects()
        .storage()
        .meta
        .redact(RedactTxn {
            ids: ids.to_vec(),
            reason: "test".to_owned(),
            actor: "github:admin".to_owned(),
            at: now,
            purge_after: now + Duration::from_secs(3600),
        })
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn blame_over_redacted_history_is_deterministic() {
    // Two independent stacks with the same history; the content of commit 1's `f` is redacted
    // in both before any blame is derived.
    let s1 = stack();
    let (ids1, m, trees) = small_history(&s1).await;
    let s2 = stack();
    let (ids2, _, _) = small_history(&s2).await;
    assert_eq!(ids1, ids2, "same inputs give the same changeset ids");
    let redacted_content = trees[1][b"f".as_slice()].content.clone();
    let blob = ObjectId::hash_blob(&redacted_content);
    redact(&s1, &[blob]).await;
    redact(&s2, &[blob]).await;

    let last = *ids1.last().unwrap();
    let a = blame::blame(&s1.engine, &last, &p("f"), None)
        .await
        .unwrap();
    let b = blame::blame(&s2.engine, &last, &p("f"), None)
        .await
        .unwrap();
    assert_eq!(a, b);
    let redacted: BTreeSet<Vec<u8>> = [redacted_content].into_iter().collect();
    assert_eq!(a, model_ranges(m.blame_redacted(3, b"f", &redacted), &ids1));
    // Redaction changed the attribution: `X` is now attributed to commit 2, whose parent's
    // content reads as empty.
    assert_ne!(a, model_ranges(m.blame(3, b"f"), &ids1));
    // Blame of the redacted version itself: empty.
    assert!(
        blame::blame(&s1.engine, &ids1[1], &p("f"), None)
            .await
            .unwrap()
            .is_empty()
    );
    // Byte-identical stored values on both stacks.
    let (node, _) = scion_derived::history::node_for_path(s1.engine.ctx(), &last, &p("f"))
        .await
        .unwrap()
        .unwrap();
    let v1 = s1
        .engine
        .get_stored(blame::NAME, None, node.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let v2 = s2
        .engine
        .get_stored(blame::NAME, None, node.as_bytes())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(v1, v2);
}
