//! SPEC 19.10 sparse object set, inaccessible subtrees and warm-cache status; SPEC 19.18
//! sparse-checkout and clean-status complexity gates (P18).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod checkout_support;

use bytes::Bytes;
use checkout_support::*;
use scion_core::{Kind, Mode, ObjectId};
use scion_object::{Canonical, Name, Tree, TreeEntry};
use scion_testkit::upstream_methods;
use scion_wc::{WcOptions, WorkingCopy};
use std::collections::BTreeSet;

fn sample() -> Files {
    files(&[
        ("a/x.txt", 1, "a-x\n"),
        ("a/b/y.txt", 1, "a-b-y\n"),
        ("a/b/c/d1.txt", 1, "d1\n"),
        ("a/b/c/run.sh", 2, "#!/bin/sh\necho\n"),
        ("a/b/c/sub/d2.txt", 1, "d2\n"),
        ("a/b/c/sub/link", 3, "../d1.txt"),
        ("a/b/e/z.txt", 1, "z\n"),
        ("other/q.txt", 1, "q\n"),
        ("top.txt", 1, "top\n"),
    ])
}

fn requested_ids(env: &Env) -> Vec<ObjectId> {
    env.up
        .requests_for(upstream_methods::GET_OBJECTS)
        .into_iter()
        .flat_map(|r| r.ids)
        .collect()
}

#[tokio::test]
async fn sparse_checkout_requests_exactly_path_trees_and_objects_under_d() {
    let env = Env::start().await;
    let f = sample();
    env.commit(&f, "main");
    let mut expected: BTreeSet<ObjectId> = ["", "a", "a/b", "a/b/c", "a/b/c/sub"]
        .iter()
        .map(|d| dir_tree_id(&f, d))
        .collect();
    for (p, (_, c)) in &f {
        if p.starts_with(b"a/b/c/") {
            expected.insert(ObjectId::hash_blob(c));
        }
    }
    let dir = tmp();
    let root = dir.path().join("wc");
    let wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        vec![rp("a/b/c")],
        WcOptions::default(),
    )
    .await
    .unwrap();
    let ids = requested_ids(&env);
    let set: BTreeSet<ObjectId> = ids.iter().copied().collect();
    assert_eq!(
        set, expected,
        "exactly the path trees plus the objects under D"
    );
    assert_eq!(ids.len(), set.len(), "no object requested twice");
    assert_eq!(wc.fetched_objects(), expected);

    // Directories on the way exist; their direct files are not materialized (SPEC 11.2).
    assert!(root.join("a/b").is_dir());
    assert!(!root.join("a/x.txt").exists());
    assert!(!root.join("a/b/y.txt").exists());
    assert!(!root.join("top.txt").exists());
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    assert_eq!(snapshot_files(&snap), materialize(&f, &[b"a/b/c".to_vec()]));

    // The reusable scenario reports the same set.
    let s = env.scenario();
    let via_scenario = scion_wc::testing::sparse_object_set(&s, "main", &rp("a/b/c"))
        .await
        .unwrap();
    assert_eq!(via_scenario, expected);
}

#[tokio::test]
async fn sparse_checkout_of_a_file_prefix() {
    let env = Env::start().await;
    let f = sample();
    env.commit(&f, "main");
    let dir = tmp();
    let root = dir.path().join("wc");
    let mut wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        vec![rp("a/b/y.txt"), rp("top.txt")],
        WcOptions::default(),
    )
    .await
    .unwrap();
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    assert_eq!(
        snapshot_files(&snap),
        materialize(&f, &[b"a/b/y.txt".to_vec(), b"top.txt".to_vec()])
    );
    assert!(wc.status().await.unwrap().is_clean());
}

#[tokio::test]
async fn warm_cache_status_makes_zero_server_calls() {
    let env = Env::start().await;
    env.commit(&sample(), "main");
    let s = env.scenario();
    let (calls, hashed) = scion_wc::testing::clean_status_zero_calls(&s, "main")
        .await
        .unwrap();
    assert_eq!(calls, 0);
    assert_eq!(hashed, 0);

    // A reopened handle also needs no server call (trees and the root mapping are on disk).
    let dir = tmp();
    let root = dir.path().join("wc");
    let mut wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .unwrap();
    wc.status().await.unwrap();
    drop(wc);
    let mut reopened = WorkingCopy::open(&root, env.client.clone(), WcOptions::default()).unwrap();
    env.up.reset_requests();
    let before = env.client.calls();
    let r = reopened.status().await.unwrap();
    assert!(r.is_clean());
    assert_eq!(env.client.calls(), before);
    assert!(env.up.requests().is_empty());

    // Changing one file hashes only that file.
    std::fs::write(root.join("top.txt"), b"changed\n").unwrap();
    let r = reopened.status().await.unwrap();
    assert_eq!(r.modified, vec![rp("top.txt")]);
    assert_eq!(r.hashed, 1);
    assert_eq!(env.client.calls(), before);
    let d = reopened.diff().await.unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].status, "modified");
    assert_eq!(d[0].path, "top.txt");
    assert_eq!(
        d[0].new_id.as_deref(),
        Some(ObjectId::hash_blob(b"changed\n").to_text().as_str())
    );
}

#[tokio::test]
async fn inaccessible_subtrees_are_recorded_and_excluded_from_status() {
    let env = Env::start().await;
    let f = sample();
    env.commit(&f, "main");
    env.up.deny(PRINCIPAL, dir_tree_id(&f, "a/b/e"));
    env.up.deny(PRINCIPAL, ObjectId::hash_blob(b"q\n"));
    let dir = tmp();
    let root = dir.path().join("wc");
    let mut wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        wc.state().inaccessible,
        vec![rp("a/b/e"), rp("other/q.txt")]
    );
    assert!(!root.join("a/b/e").exists());
    assert!(!root.join("other/q.txt").exists());
    assert!(root.join("a/b/y.txt").exists());
    let r = wc.status().await.unwrap();
    assert!(
        r.is_clean(),
        "inaccessible paths are not reported deleted: {r:?}"
    );
    // Local files under an inaccessible prefix are not reported either.
    std::fs::create_dir_all(root.join("a/b/e")).unwrap();
    std::fs::write(root.join("a/b/e/local.txt"), b"x").unwrap();
    assert!(wc.status().await.unwrap().is_clean());
}

/// Graft a fixed directory into a shared-subtree repository; returns the changeset.
fn shared_with_fixed(env: &Env, files_n: u64) -> ObjectId {
    let repo = scion_testkit::shared_subtree_repo(files_n, 3);
    let objs = repo.encoded_objects();
    let root_id = repo.root_tree();
    let mut root_tree = None;
    for (id, kind, raw) in &objs {
        if *id == root_id {
            root_tree = Some(Tree::decode(raw).unwrap());
        }
        assert!(matches!(kind, Kind::Blob | Kind::Tree));
        env.up.insert_object(*id, raw.clone(), false);
    }
    let fixed = files(&[
        ("f1.txt", 1, "one\n"),
        ("f2.txt", 1, "two\n"),
        ("sub/f3.txt", 1, "three\n"),
        ("sub/x.sh", 2, "#!/bin/sh\n"),
    ]);
    let fixed_root = env.seed(&fixed);
    let mut root_tree = root_tree.unwrap();
    root_tree.entries.push(TreeEntry {
        name: Name::new(b"fixed").unwrap(),
        mode: Mode::Tree,
        id: fixed_root,
        size: 0,
    });
    root_tree
        .entries
        .sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let raw = root_tree.encode();
    let new_root = root_tree.id();
    env.up.insert_object(new_root, Bytes::from(raw), false);
    let cs = env.changeset(new_root);
    env.up.set_ref("main", cs, 1);
    cs
}

async fn sparse_fixed_count(files_n: u64) -> usize {
    let env = Env::start().await;
    shared_with_fixed(&env, files_n);
    let dir = tmp();
    let root = dir.path().join("wc");
    let mut wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        vec![rp("fixed")],
        WcOptions::default(),
    )
    .await
    .unwrap();
    let n = wc.fetched_objects().len();
    // Clean status reads zero objects upstream and hashes only racy files (SPEC 19.18).
    wc.status().await.unwrap();
    let before = env.client.calls();
    let r = wc.status().await.unwrap();
    assert!(r.is_clean());
    assert_eq!(r.hashed, 0);
    assert_eq!(env.client.calls(), before);
    n
}

#[tokio::test]
async fn gate_sparse_checkout_object_count_is_independent_of_repository_size() {
    let small = sparse_fixed_count(1_000).await;
    let large = sparse_fixed_count(100_000).await;
    // root + fixed + fixed/sub trees, plus 4 blobs.
    assert_eq!(small, 7);
    assert_eq!(small, large);
}

#[tokio::test]
#[ignore = "full_: spec-sized 10^7-file gate (SCION_FULL)"]
async fn full_gate_sparse_checkout_object_count_at_ten_million_files() {
    let small = sparse_fixed_count(1_000).await;
    let huge = sparse_fixed_count(10_000_000).await;
    assert_eq!(small, huge);
}

#[tokio::test]
async fn gate_clean_full_status_hashes_nothing_and_calls_nothing() {
    let env = Env::start().await;
    let repo = scion_testkit::shared_subtree_repo(1_000, 3);
    for (id, _, raw) in repo.encoded_objects() {
        env.up.insert_object(id, raw, false);
    }
    let cs = env.changeset(repo.root_tree());
    env.up.set_ref("main", cs, 1);
    let s = env.scenario();
    let (calls, hashed) = scion_wc::testing::clean_status_zero_calls(&s, "main")
        .await
        .unwrap();
    assert_eq!((calls, hashed), (0, 0));
}
