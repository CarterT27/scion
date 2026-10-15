//! SPEC 11.3 checkout into a directory that already holds files (P18 review): pre-existing
//! files with different content are never silently overwritten (refused before any write);
//! files that already hold their target content are adopted as clean.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod checkout_support;

use checkout_support::*;
use scion_wc::{WcOptions, WorkingCopy};

fn tree() -> Files {
    files(&[
        ("a/one.txt", 1, "one\n"),
        ("a/two.txt", 1, "two\n"),
        ("top.txt", 1, "top\n"),
    ])
}

#[tokio::test]
async fn differing_pre_existing_files_are_refused_before_any_write() {
    let env = Env::start().await;
    env.commit(&tree(), "main");
    let dir = tmp();
    let root = dir.path().join("wc");
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::write(root.join("a/two.txt"), b"my own work\n").unwrap();
    let err = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(err.subcode(), Some(WorkingCopy::SUBCODE_DIRTY));
    assert_eq!(
        std::fs::read(root.join("a/two.txt")).unwrap(),
        b"my own work\n"
    );
    assert_eq!(ls(&root), vec!["a"], "nothing written, .scion removed");
    assert_eq!(ls(&root.join("a")), vec!["two.txt"]);
}

#[tokio::test]
async fn identical_pre_existing_files_are_adopted() {
    let env = Env::start().await;
    env.commit(&tree(), "main");
    let dir = tmp();
    let root = dir.path().join("wc");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("top.txt"), b"top\n").unwrap();
    std::fs::write(root.join("untracked.txt"), b"keep me\n").unwrap();
    let mut wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .unwrap();
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    let mut expected = tree();
    expected.extend(files(&[("untracked.txt", 1, "keep me\n")]));
    assert_eq!(snapshot_files(&snap), expected);
    let st = wc.status().await.unwrap();
    assert_eq!(st.added, vec![rp("untracked.txt")]);
    assert!(st.modified.is_empty() && st.deleted.is_empty());
}

#[tokio::test]
async fn warm_status_keeps_the_index_until_a_clean_stat_change_or_deletion() {
    let env = Env::start().await;
    env.commit(&tree(), "main");
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
    // The first status settles any checkout entries written within the same timestamp tick.
    assert!(wc.status().await.unwrap().is_clean());
    let index = root.join(".scion/index");
    let before = std::fs::read(&index).unwrap();
    let before_md = std::fs::metadata(&index).unwrap();
    env.up.reset_requests();
    assert_eq!(wc.status().await.unwrap().hashed, 0);
    assert_eq!(std::fs::read(&index).unwrap(), before);
    use std::os::unix::fs::MetadataExt;
    assert_eq!(std::fs::metadata(&index).unwrap().ino(), before_md.ino());
    assert!(
        env.up.requests().is_empty(),
        "cached status must stay offline"
    );

    // Rewriting unchanged content updates the stat cache and its racy timestamp.
    std::fs::write(root.join("top.txt"), b"top\n").unwrap();
    assert_eq!(wc.status().await.unwrap().hashed, 1);
    assert_ne!(std::fs::read(&index).unwrap(), before);
    assert!(wc.status().await.unwrap().is_clean());
    let before = std::fs::read(&index).unwrap();
    assert_eq!(wc.status().await.unwrap().hashed, 0);
    assert_eq!(std::fs::read(&index).unwrap(), before);

    std::fs::remove_file(root.join("top.txt")).unwrap();
    assert_eq!(wc.status().await.unwrap().deleted, vec![rp("top.txt")]);
    let idx = scion_wc::index::StatCache::load(&root.join(".scion")).unwrap();
    assert!(!idx.entries.contains_key(&rp("top.txt")));
}
