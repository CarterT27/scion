//! SPEC 4.4 / 11.3 / 12.6 goto edge cases (P18 review): case collisions introduced by a goto
//! are reported and never overwrite a staying file; untracked files that a new path would
//! overwrite under case folding are dirty files in the way; subtrees and blobs that become
//! unreadable are recorded as inaccessible and leave the materialization exactly as a fresh
//! checkout would; and a crash after such a goto is recovered identically.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod checkout_support;

use checkout_support::*;
use scion_core::{FaultAction, FaultRegistry, ObjectId};
use scion_wc::points;
use scion_wc::{WcOptions, WorkingCopy};

fn ci() -> WcOptions {
    WcOptions {
        force_case_insensitive: true,
        ..WcOptions::default()
    }
}

#[tokio::test]
async fn goto_reports_case_collisions_and_keeps_the_staying_file() {
    let env = Env::start().await;
    env.commit(
        &files(&[("readme", 1, "lower\n"), ("Dir/x.txt", 1, "x\n")]),
        "v1",
    );
    env.commit(
        &files(&[
            ("README", 1, "upper\n"),
            ("readme", 1, "lower\n"),
            ("Dir/x.txt", 1, "x\n"),
            ("dir/y.txt", 1, "y\n"),
            ("fine.txt", 1, "fine\n"),
        ]),
        "v2",
    );
    let dir = tmp();
    let root = dir.path().join("wc");
    let mut wc = WorkingCopy::checkout(env.client.clone(), &root, "v1", Vec::new(), ci())
        .await
        .unwrap();
    wc.goto("v2", false).await.unwrap();
    assert_eq!(
        wc.collisions(),
        &[
            (rp("Dir/x.txt"), rp("dir/y.txt")),
            (rp("readme"), rp("README")),
        ]
    );
    assert_eq!(std::fs::read(root.join("readme")).unwrap(), b"lower\n");
    assert_eq!(std::fs::read(root.join("fine.txt")).unwrap(), b"fine\n");
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    let names: Vec<String> = snap
        .keys()
        .map(|p| String::from_utf8_lossy(p.as_bytes()).into_owned())
        .collect();
    assert_eq!(names, vec!["Dir/x.txt", "fine.txt", "readme"]);
    assert!(wc.state().pending.is_none());

    // The skipped entry is absent locally, so status reports it deleted.
    let st = wc.status().await.unwrap();
    assert_eq!(st.deleted, vec![rp("README"), rp("dir/y.txt")]);

    // A case-only rename is no collision: the old name is removed first. (`force`, because
    // the target modifies README, which is locally deleted.)
    env.commit(&files(&[("README", 1, "lower\n")]), "v3");
    wc.goto("v3", true).await.unwrap();
    assert!(wc.collisions().is_empty());
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    assert_eq!(snapshot_files(&snap), files(&[("README", 1, "lower\n")]));
}

#[tokio::test]
async fn goto_refuses_untracked_files_that_collide_by_case() {
    let env = Env::start().await;
    env.commit(&files(&[("a.txt", 1, "a\n")]), "v1");
    env.commit(
        &files(&[("a.txt", 1, "a\n"), ("notes.txt", 1, "n\n")]),
        "v2",
    );
    let dir = tmp();
    let root = dir.path().join("wc");
    let mut wc = WorkingCopy::checkout(env.client.clone(), &root, "v1", Vec::new(), ci())
        .await
        .unwrap();
    std::fs::write(root.join("NOTES.txt"), b"mine\n").unwrap();
    let err = wc.goto("v2", false).await.unwrap_err();
    assert_eq!(err.subcode(), Some(WorkingCopy::SUBCODE_DIRTY));
    assert_eq!(std::fs::read(root.join("NOTES.txt")).unwrap(), b"mine\n");
    assert!(wc.state().pending.is_none());
}

fn secret_v1() -> Files {
    files(&[
        ("pub.txt", 1, "public\n"),
        ("sec/a.txt", 1, "one\n"),
        ("f.txt", 1, "old\n"),
    ])
}

fn secret_v2() -> Files {
    files(&[
        ("pub.txt", 1, "public\n"),
        ("sec/a.txt", 1, "two\n"),
        ("sec/b.txt", 1, "bee\n"),
        ("f.txt", 1, "secret\n"),
    ])
}

/// v1 readable; in v2 the `sec` subtree and the new `f.txt` blob are denied.
async fn secret_env() -> Env {
    let env = Env::start().await;
    env.commit(&secret_v1(), "v1");
    env.commit(&secret_v2(), "v2");
    env.up.deny(PRINCIPAL, dir_tree_id(&secret_v2(), "sec"));
    env.up.deny(PRINCIPAL, ObjectId::hash_blob(b"secret\n"));
    env
}

#[tokio::test]
async fn goto_into_denied_content_matches_a_fresh_checkout() {
    let env = secret_env().await;
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
    wc.goto("v2", false).await.unwrap();
    assert_eq!(wc.state().inaccessible, vec![rp("f.txt"), rp("sec")]);
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    let fresh = scion_wc::testing::checkout_snapshot(&env.scenario(), "v2", &[])
        .await
        .unwrap();
    assert_eq!(snap, fresh, "goto result equals a fresh checkout");
    assert_eq!(snapshot_files(&snap), files(&[("pub.txt", 1, "public\n")]));
    assert!(wc.status().await.unwrap().is_clean());

    // Back to readable content: everything is materialized again and nothing is stale.
    wc.goto("v1", false).await.unwrap();
    assert!(wc.state().inaccessible.is_empty());
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    assert_eq!(snapshot_files(&snap), secret_v1());
    assert!(wc.status().await.unwrap().is_clean());
}

#[tokio::test]
async fn goto_into_denied_content_protects_dirty_files() {
    let env = secret_env().await;
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
    std::fs::write(root.join("sec/a.txt"), b"local\n").unwrap();
    let err = wc.goto("v2", false).await.unwrap_err();
    assert_eq!(err.subcode(), Some(WorkingCopy::SUBCODE_DIRTY));
    assert_eq!(std::fs::read(root.join("sec/a.txt")).unwrap(), b"local\n");
}

#[tokio::test]
async fn crashed_goto_into_denied_content_recovers_identically() {
    for point in [points::GOTO_AFTER_JOURNAL, points::GOTO_MID_WRITE] {
        let env = secret_env().await;
        let dir = tmp();
        let root = dir.path().join("wc");
        let faults = FaultRegistry::new();
        let opts = WcOptions {
            faults: faults.clone(),
            ..WcOptions::default()
        };
        let mut wc =
            WorkingCopy::checkout(env.client.clone(), &root, "v1", Vec::new(), opts.clone())
                .await
                .unwrap();
        faults.arm(point, FaultAction::Crash, Some(1));
        assert!(wc.goto("v2", false).await.is_err(), "{point}");
        faults.reset();
        let mut reopened = WorkingCopy::open(&root, env.client.clone(), opts).unwrap();
        assert!(reopened.recover().await.unwrap().is_some(), "{point}");
        assert!(reopened.state().pending.is_none());
        assert_eq!(reopened.state().inaccessible, vec![rp("f.txt"), rp("sec")]);
        let snap = scion_wc::testing::snapshot(&root).unwrap();
        assert_eq!(
            snapshot_files(&snap),
            files(&[("pub.txt", 1, "public\n")]),
            "{point}"
        );
        assert!(reopened.status().await.unwrap().is_clean(), "{point}");
    }
}
