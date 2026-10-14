//! SPEC 11.3 / 19.10 goto: only differing paths are touched (identical subtrees pruned and
//! never fetched), unrelated dirty files are preserved, dirty files the diff would overwrite
//! abort unless forced, and a crash at every `wc.goto.*` point is recovered by the next
//! command (P18).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod checkout_support;

use checkout_support::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use scion_core::{FaultAction, FaultRegistry, ObjectId};
use scion_testkit::{CorpusSpec, generate, upstream_methods};
use scion_wc::points;
use scion_wc::state::PendingOperation;
use scion_wc::{WcOptions, WorkingCopy};
use std::collections::BTreeSet;

fn v1() -> Files {
    files(&[
        ("a/del.txt", 1, "delete me\n"),
        ("a/keep.txt", 1, "keep\n"),
        ("a/mod.txt", 1, "one\n"),
        ("deep/x/y/z.txt", 1, "deep\n"),
        ("exec.sh", 1, "#!/bin/sh\n"),
        ("flip", 1, "a file\n"),
        ("link", 3, "a/keep.txt"),
        ("other/unchanged.txt", 1, "same\n"),
    ])
}

fn v2() -> Files {
    files(&[
        ("a/keep.txt", 1, "keep\n"),
        ("a/mod.txt", 1, "two\n"),
        ("a/new.txt", 1, "new\n"),
        ("deep/x/y/z.txt", 1, "deep\n"),
        ("exec.sh", 2, "#!/bin/sh\n"),
        ("flip/inner.txt", 1, "now a dir\n"),
        ("link", 3, "a/mod.txt"),
        ("other/unchanged.txt", 1, "same\n"),
    ])
}

async fn env_v1_v2() -> Env {
    let env = Env::start().await;
    env.commit(&v1(), "v1");
    env.commit(&v2(), "v2");
    env
}

#[tokio::test]
async fn goto_touches_only_differing_paths() {
    let env = env_v1_v2().await;
    let s = env.scenario();
    let changed = scion_wc::testing::goto_touches_only_diff(&s, "v1", "v2")
        .await
        .unwrap();
    let expected: Vec<_> = [
        "a/del.txt",
        "a/mod.txt",
        "a/new.txt",
        "exec.sh",
        "flip",
        "flip/inner.txt",
        "link",
    ]
    .iter()
    .map(|p| rp(p))
    .collect();
    assert_eq!(changed, expected);
    // And back again.
    let back = scion_wc::testing::goto_touches_only_diff(&s, "v2", "v1")
        .await
        .unwrap();
    assert_eq!(back, expected);
}

#[tokio::test]
async fn goto_prunes_identical_subtrees_and_fetches_only_differences() {
    let env = env_v1_v2().await;
    let dir = tmp();
    let root = dir.path().join("wc");
    let mut wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "v1",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .unwrap();
    env.up.reset_requests();
    let report = wc.goto("v2", false).await.unwrap();
    assert_eq!(report.written, 5); // mod, new, exec.sh, flip/inner.txt, link
    assert_eq!(report.removed, 2); // del, flip
    assert!(report.preserved_dirty.is_empty());
    let reqs = env.up.requests_for(upstream_methods::GET_OBJECTS);
    // The first request is the redaction-refresh probe of every materialized base blob
    // (zero-length range reads, SPEC 13.7); content fetches follow.
    let probe: BTreeSet<ObjectId> = reqs[0].ids.iter().copied().collect();
    let base_blobs: BTreeSet<ObjectId> =
        v1().values().map(|(_, c)| ObjectId::hash_blob(c)).collect();
    assert_eq!(probe, base_blobs);
    let requested: BTreeSet<ObjectId> = reqs[1..].iter().flat_map(|r| r.ids.clone()).collect();
    let f2 = v2();
    let mut expected: BTreeSet<ObjectId> = ["", "a", "flip"]
        .iter()
        .map(|d| dir_tree_id(&f2, d))
        .collect();
    for p in ["a/mod.txt", "a/new.txt", "flip/inner.txt", "link"] {
        expected.insert(ObjectId::hash_blob(&f2[p.as_bytes()].1));
    }
    // exec.sh: same blob as before (mode change only) - re-fetched for the rewrite.
    expected.insert(ObjectId::hash_blob(b"#!/bin/sh\n"));
    assert_eq!(requested, expected);
    assert!(!requested.contains(&dir_tree_id(&f2, "deep")));
    assert!(!requested.contains(&dir_tree_id(&f2, "other")));
    assert_eq!(wc.state().base.to_text(), report.to);
    assert_eq!(wc.state().tracking_ref.as_ref().unwrap().as_str(), "v2");
    // Status is clean and needs no server call afterwards.
    wc.status().await.unwrap();
    let before = env.client.calls();
    assert!(wc.status().await.unwrap().is_clean());
    assert_eq!(env.client.calls(), before);
}

#[tokio::test]
async fn goto_preserves_unrelated_dirty_files_and_refuses_overwrites() {
    let env = env_v1_v2().await;
    let s = env.scenario();
    scion_wc::testing::goto_dirty_protection(
        &s,
        "v1",
        "v2",
        &rp("a/mod.txt"),
        &rp("other/unchanged.txt"),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn goto_refuses_untracked_files_in_the_way() {
    let env = env_v1_v2().await;
    let dir = tmp();
    let root = dir.path().join("wc");
    let mut wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "v1",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .unwrap();
    // An untracked file at a path v2 adds.
    std::fs::write(root.join("a/new.txt"), b"mine\n").unwrap();
    let err = wc.goto("v2", false).await.unwrap_err();
    assert_eq!(err.subcode(), Some(WorkingCopy::SUBCODE_DIRTY));
    assert_eq!(std::fs::read(root.join("a/new.txt")).unwrap(), b"mine\n");
    // A locally deleted file that the target deletes too is not a conflict.
    std::fs::remove_file(root.join("a/new.txt")).unwrap();
    std::fs::remove_file(root.join("a/del.txt")).unwrap();
    wc.goto("v2", false).await.unwrap();
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    assert_eq!(snapshot_files(&snap), v2());
}

#[tokio::test]
async fn goto_through_random_history_matches_the_model() {
    let env = Env::start().await;
    let repo = generate(&CorpusSpec {
        files: 50,
        depth: 4,
        fanout: 3,
        history_len: 10,
        seed: 42,
        ..CorpusSpec::default()
    });
    let trees: Vec<Files> = repo
        .commits
        .iter()
        .map(|c| {
            from_triples(
                c.tree
                    .iter()
                    .map(|(p, m)| (p.clone(), m.mode, m.content.clone())),
            )
        })
        .collect();
    for (i, t) in trees.iter().enumerate() {
        env.commit(t, &format!("c{i}"));
    }
    let dir = tmp();
    let root = dir.path().join("wc");
    let mut wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "c0",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .unwrap();
    let mut rng = StdRng::seed_from_u64(9);
    let steps = scion_core::test_support::proptest_cases(12);
    for _ in 0..steps {
        let i = rng.random_range(0..trees.len());
        wc.goto(&format!("c{i}"), false).await.unwrap();
        let snap = scion_wc::testing::snapshot(&root).unwrap();
        assert_eq!(snapshot_files(&snap), trees[i], "after goto c{i}");
        assert!(wc.status().await.unwrap().is_clean());
    }
}

#[tokio::test]
async fn crash_at_every_goto_point_is_recovered_by_the_next_command() {
    for point in [points::GOTO_AFTER_JOURNAL, points::GOTO_MID_WRITE] {
        let env = env_v1_v2().await;
        let s = env.scenario();
        scion_wc::testing::goto_crash_recovery(&s, "v1", "v2", point)
            .await
            .unwrap_or_else(|e| panic!("{point}: {e}"));
    }
}

#[tokio::test]
async fn crashed_goto_leaves_a_pending_operation_that_goto_completes_first() {
    let env = env_v1_v2().await;
    let faults = FaultRegistry::new();
    let opts = WcOptions {
        faults: faults.clone(),
        ..WcOptions::default()
    };
    let dir = tmp();
    let root = dir.path().join("wc");
    let mut wc = WorkingCopy::checkout(env.client.clone(), &root, "v1", Vec::new(), opts.clone())
        .await
        .unwrap();
    let v1_id = wc.state().base;
    // Fail right after the first file operation: some files are at v2, others at v1.
    faults.arm(
        points::GOTO_MID_WRITE,
        FaultAction::Error(scion_core::ErrorCode::Unavailable),
        Some(1),
    );
    assert!(wc.goto("v2", false).await.is_err());
    let st = scion_wc::state::load(&root.join(".scion")).unwrap();
    match st.pending {
        Some(PendingOperation::Goto { from, .. }) => assert_eq!(from, v1_id),
        other => panic!("expected a pending goto, got {other:?}"),
    }
    assert_eq!(st.base, v1_id);
    faults.disarm_all();
    faults.reset();
    // The next command is a goto back to v1: the pending goto is completed first, then the
    // new goto runs.
    let mut wc = WorkingCopy::open(&root, env.client.clone(), opts).unwrap();
    wc.goto("v1", false).await.unwrap();
    assert!(wc.state().pending.is_none());
    assert_eq!(wc.state().base, v1_id);
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    assert_eq!(snapshot_files(&snap), v1());
    // recover() with nothing pending is a no-op.
    assert_eq!(wc.recover().await.unwrap(), None);
}
