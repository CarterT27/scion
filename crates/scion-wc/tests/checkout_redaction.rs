//! SPEC 13.7 / 19.13 working copy: checkout writes placeholders for redacted blobs, status
//! treats unmodified placeholders as clean, newly redacted clean files are replaced on the next
//! mutating command, and dirty files are untouched and reported (P18).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod checkout_support;

use checkout_support::*;
use scion_core::ObjectId;
use scion_wc::{WcOptions, WorkingCopy, placeholder_bytes};

fn tree() -> Files {
    files(&[
        ("secret.txt", 1, "api key\n"),
        ("run.sh", 2, "#!/bin/sh\nsecret\n"),
        ("clean.txt", 1, "becomes redacted\n"),
        ("dirty.txt", 1, "also redacted\n"),
        ("keep.txt", 1, "keep\n"),
        ("dir/link", 3, "../keep.txt"),
    ])
}

fn id(s: &str) -> ObjectId {
    ObjectId::hash_blob(s.as_bytes())
}

#[tokio::test]
async fn checkout_writes_placeholders_and_status_treats_them_as_clean() {
    let env = Env::start().await;
    env.commit(&tree(), "main");
    env.up.redact(id("api key\n"));
    env.up.redact(id("#!/bin/sh\nsecret\n"));
    env.up.redact(id("../keep.txt")); // a redacted symlink target
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
        std::fs::read(root.join("secret.txt")).unwrap(),
        placeholder_bytes(&id("api key\n"))
    );
    assert_eq!(
        std::fs::read(root.join("run.sh")).unwrap(),
        placeholder_bytes(&id("#!/bin/sh\nsecret\n"))
    );
    assert_eq!(
        std::fs::read(root.join("dir/link")).unwrap(),
        placeholder_bytes(&id("../keep.txt"))
    );
    let idx = scion_wc::index::StatCache::load(&root.join(".scion")).unwrap();
    assert!(idx.entries[&rp("secret.txt")].redacted);
    assert!(idx.entries[&rp("dir/link")].redacted);
    assert!(!idx.entries[&rp("keep.txt")].redacted);
    let r = wc.status().await.unwrap();
    assert!(r.is_clean(), "{r:?}");
    // Editing a placeholder makes it dirty (SPEC 13.7).
    std::fs::write(root.join("secret.txt"), b"rotated key\n").unwrap();
    let r = wc.status().await.unwrap();
    assert_eq!(r.modified, vec![rp("secret.txt")]);
    assert_eq!(r.redacted_dirty, vec![rp("secret.txt")]);
}

#[tokio::test]
async fn newly_redacted_clean_files_are_replaced_on_the_next_mutating_command() {
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
    assert_eq!(
        std::fs::read(root.join("clean.txt")).unwrap(),
        b"becomes redacted\n"
    );
    std::fs::write(root.join("dirty.txt"), b"local edit\n").unwrap();
    env.up.redact(id("becomes redacted\n"));
    env.up.redact(id("also redacted\n"));

    // status is not mutating: nothing is replaced.
    wc.status().await.unwrap();
    assert_eq!(
        std::fs::read(root.join("clean.txt")).unwrap(),
        b"becomes redacted\n"
    );

    // The next mutating command (a no-op goto to the same revision) replaces clean copies.
    wc.goto("main", false).await.unwrap();
    assert_eq!(
        std::fs::read(root.join("clean.txt")).unwrap(),
        placeholder_bytes(&id("becomes redacted\n"))
    );
    assert_eq!(
        std::fs::read(root.join("dirty.txt")).unwrap(),
        b"local edit\n"
    );
    assert_eq!(std::fs::read(root.join("keep.txt")).unwrap(), b"keep\n");
    let r = wc.status().await.unwrap();
    assert_eq!(r.modified, vec![rp("dirty.txt")]);

    // Dirty files are reported (and still untouched).
    let reported = wc.refresh_redactions().await.unwrap();
    assert_eq!(reported, vec![rp("dirty.txt")]);
    assert_eq!(
        std::fs::read(root.join("dirty.txt")).unwrap(),
        b"local edit\n"
    );
    // Placeholders are not probed again.
    let idx = scion_wc::index::StatCache::load(&root.join(".scion")).unwrap();
    assert!(idx.entries[&rp("clean.txt")].redacted);
}

#[tokio::test]
async fn redaction_refresh_happens_before_sparse_changes() {
    let env = Env::start().await;
    env.commit(&tree(), "main");
    let dir = tmp();
    let root = dir.path().join("wc");
    let mut wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        vec![rp("keep.txt"), rp("clean.txt")],
        WcOptions::default(),
    )
    .await
    .unwrap();
    env.up.redact(id("becomes redacted\n"));
    wc.sparse_add(&[rp("dir")]).await.unwrap();
    assert_eq!(
        std::fs::read(root.join("clean.txt")).unwrap(),
        placeholder_bytes(&id("becomes redacted\n"))
    );
    assert!(wc.status().await.unwrap().is_clean());
}
