//! SPEC 11.2 / 19.10 sparse add/remove: exactly the affected files are materialized or
//! removed, dirty removal is refused unless forced, and a crash at `wc.sparse.mid_write` is
//! recovered by the next command (P18).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod checkout_support;

use checkout_support::*;
use scion_core::{ErrorCode, FaultAction, FaultRegistry};
use scion_wc::points;
use scion_wc::state::PendingOperation;
use scion_wc::{WcOptions, WorkingCopy};

fn tree() -> Files {
    files(&[
        ("docs/guide.md", 1, "guide\n"),
        ("docs/img/logo.svg", 1, "<svg/>\n"),
        ("src/lib.rs", 1, "lib\n"),
        ("src/bin/main.rs", 1, "main\n"),
        ("src/run.sh", 2, "#!/bin/sh\n"),
        ("tools/x/y.txt", 1, "y\n"),
        ("tools/link", 3, "x/y.txt"),
        ("top.txt", 1, "top\n"),
    ])
}

#[tokio::test]
async fn sparse_add_and_remove_change_exactly_the_affected_files() {
    let env = Env::start().await;
    env.commit(&tree(), "main");
    let s = env.scenario();
    scion_wc::testing::sparse_add_remove(&s, "main", &rp("src"), &rp("tools"))
        .await
        .unwrap();
    scion_wc::testing::sparse_add_remove(&s, "main", &rp("docs/img"), &rp("top.txt"))
        .await
        .unwrap();
}

#[tokio::test]
async fn sparse_changes_touch_nothing_else() {
    let env = Env::start().await;
    let f = tree();
    env.commit(&f, "main");
    let dir = tmp();
    let root = dir.path().join("wc");
    let mut wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        vec![rp("src")],
        WcOptions::default(),
    )
    .await
    .unwrap();
    let before = scion_wc::testing::stat_map(&root).unwrap();
    wc.sparse_add(&[rp("docs")]).await.unwrap();
    assert_eq!(wc.state().sparse, vec![rp("src"), rp("docs")]);
    let after = scion_wc::testing::stat_map(&root).unwrap();
    for (p, st) in &before {
        assert_eq!(after.get(p), Some(st), "untouched by sparse add");
    }
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    assert_eq!(
        snapshot_files(&snap),
        materialize(&f, &[b"src".to_vec(), b"docs".to_vec()])
    );
    assert!(wc.status().await.unwrap().is_clean());

    // Adding a prefix already covered changes nothing.
    wc.sparse_add(&[rp("src/bin")]).await.unwrap();
    assert_eq!(
        wc.state().sparse,
        vec![rp("src"), rp("docs"), rp("src/bin")]
    );

    wc.sparse_remove(&[rp("src")], false).await.unwrap();
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    assert_eq!(
        snapshot_files(&snap),
        materialize(&f, &[b"docs".to_vec(), b"src/bin".to_vec()])
    );
    let after_remove = scion_wc::testing::stat_map(&root).unwrap();
    assert_eq!(
        after_remove.get(&rp("docs/guide.md")),
        after.get(&rp("docs/guide.md"))
    );
    assert!(wc.status().await.unwrap().is_clean());

    // Removing a prefix that is not in the profile is an error.
    let err = wc.sparse_remove(&[rp("nope")], false).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
}

#[tokio::test]
async fn sparse_remove_refuses_dirty_files_unless_forced() {
    let env = Env::start().await;
    env.commit(&tree(), "main");
    let dir = tmp();
    let root = dir.path().join("wc");
    let mut wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        vec![rp("src"), rp("docs")],
        WcOptions::default(),
    )
    .await
    .unwrap();
    // An untracked file in the leaving region is dirty too.
    std::fs::write(root.join("docs/notes.txt"), b"mine\n").unwrap();
    let err = wc.sparse_remove(&[rp("docs")], false).await.unwrap_err();
    assert_eq!(err.subcode(), Some(WorkingCopy::SUBCODE_DIRTY));
    assert!(root.join("docs/guide.md").exists());
    assert_eq!(wc.state().sparse, vec![rp("src"), rp("docs")]);
    // A dirty file outside the leaving region does not block.
    std::fs::remove_file(root.join("docs/notes.txt")).unwrap();
    std::fs::write(root.join("src/lib.rs"), b"edited\n").unwrap();
    wc.sparse_remove(&[rp("docs")], false).await.unwrap();
    assert!(!root.join("docs").exists());
    assert_eq!(std::fs::read(root.join("src/lib.rs")).unwrap(), b"edited\n");
    // Forced removal of a modified tracked file.
    wc.sparse_add(&[rp("docs")]).await.unwrap();
    std::fs::write(root.join("docs/guide.md"), b"edited guide\n").unwrap();
    assert!(wc.sparse_remove(&[rp("docs")], false).await.is_err());
    wc.sparse_remove(&[rp("docs")], true).await.unwrap();
    assert!(!root.join("docs/guide.md").exists());
    let st = wc.status().await.unwrap();
    assert_eq!(st.modified, vec![rp("src/lib.rs")]);
}

async fn crash_case(add: bool) {
    let env = Env::start().await;
    let f = tree();
    env.commit(&f, "main");
    let faults = FaultRegistry::new();
    let opts = WcOptions {
        faults: faults.clone(),
        ..WcOptions::default()
    };
    let dir = tmp();
    let root = dir.path().join("wc");
    let initial = if add {
        vec![rp("top.txt")]
    } else {
        vec![rp("top.txt"), rp("src")]
    };
    let mut wc = WorkingCopy::checkout(env.client.clone(), &root, "main", initial, opts.clone())
        .await
        .unwrap();
    faults.arm(points::SPARSE_MID_WRITE, FaultAction::Crash, Some(1));
    let res = if add {
        wc.sparse_add(&[rp("src")]).await
    } else {
        wc.sparse_remove(&[rp("src")], false).await
    };
    assert!(res.is_err());
    let st = scion_wc::state::load(&root.join(".scion")).unwrap();
    assert!(matches!(
        st.pending,
        Some(PendingOperation::SparseChange { .. })
    ));
    faults.reset();
    let mut wc = WorkingCopy::open(&root, env.client.clone(), opts).unwrap();
    let r = wc.status().await.unwrap();
    assert!(r.is_clean(), "{r:?}");
    assert!(wc.state().pending.is_none());
    let expected: Vec<Vec<u8>> = if add {
        vec![b"top.txt".to_vec(), b"src".to_vec()]
    } else {
        vec![b"top.txt".to_vec()]
    };
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    assert_eq!(snapshot_files(&snap), materialize(&f, &expected));
}

#[tokio::test]
async fn crash_during_sparse_add_is_completed_by_the_next_command() {
    crash_case(true).await;
}

#[tokio::test]
async fn crash_during_sparse_remove_is_completed_by_the_next_command() {
    crash_case(false).await;
}
