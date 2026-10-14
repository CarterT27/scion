//! SPEC 19.10 / 16.1 class 8 crash injection at every `wc.commit.*` and `wc.rebase.*` fault
//! point against the real in-process server (P28): the crash leaves a recoverable pending
//! operation that the next command completes or rolls back, after which status is consistent
//! and the server state is valid.

mod commit_support;

use commit_support::*;
use scion_core::{FaultAction, FaultRegistry, Mode};
use scion_wc::points;
use scion_wc::state::PendingOperation;
use scion_wc::testing::{
    Recovered, checkout_at, commit_crash_recovery, crash_recovery_all, rebase_crash_recovery,
    seed_files, wc_snapshot, write_files, write_points,
};
use scion_wc::{WcOptions, WorkingCopy};

#[tokio::test(flavor = "multi_thread")]
async fn commit_crash_after_journal_rolls_back() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let got = commit_crash_recovery(&s, &main_ref(), points::COMMIT_AFTER_JOURNAL)
        .await
        .unwrap();
    // The new blob was never uploaded, so the changeset cannot have been created.
    assert_eq!(got, Recovered::RolledBack);
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_crash_after_upload_completes() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let got = commit_crash_recovery(&s, &main_ref(), points::COMMIT_AFTER_UPLOAD)
        .await
        .unwrap();
    assert_eq!(got, Recovered::Completed);
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_crash_after_create_completes() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let got = commit_crash_recovery(&s, &main_ref(), points::COMMIT_AFTER_CREATE)
        .await
        .unwrap();
    assert_eq!(got, Recovered::Completed);
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rebase_crash_points_resume() {
    for (point, want) in [
        (points::REBASE_AFTER_JOURNAL, Recovered::Completed),
        (points::REBASE_MID_MEMBER, Recovered::Completed),
        (
            points::REBASE_AFTER_CONFLICT_WRITE,
            Recovered::PausedOnConflicts,
        ),
    ] {
        let srv = server().await;
        let s = scenario(&srv, &alice()).await;
        let got = rebase_crash_recovery(&s, &main_ref(), point).await.unwrap();
        assert_eq!(got, want, "{point}");
        validate_server(&srv).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_write_point_on_one_server() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let outcomes = crash_recovery_all(&s, &main_ref()).await.unwrap();
    assert_eq!(outcomes.len(), write_points().len());
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn amend_crash_is_recovered_with_the_same_change_id() {
    for point in [points::COMMIT_AFTER_JOURNAL, points::COMMIT_AFTER_CREATE] {
        let srv = server().await;
        let s = scenario(&srv, &alice()).await;
        let faults = FaultRegistry::new();
        let mut wc = checkout_at(&s, "main", Some(faults.clone())).await.unwrap();
        write_files(&wc, &[("f", Mode::File, b"1\n")]).unwrap();
        let c = wc.commit_with_report(b"one\n", b"A", None).await.unwrap();
        write_files(&wc, &[("g", Mode::File, b"2\n")]).unwrap();
        faults.arm(point, FaultAction::Crash, Some(1));
        assert!(wc.amend(None).await.is_err());
        faults.reset();
        assert!(matches!(
            wc.state().pending,
            Some(PendingOperation::Commit { amend: true, .. })
        ));
        let root = wc.root().to_path_buf();
        drop(wc);
        let mut wc = WorkingCopy::open(&root, s.client.clone(), WcOptions::default()).unwrap();
        let what = wc.recover().await.unwrap();
        assert!(what.is_some());
        assert!(wc.state().pending.is_none());
        if wc.state().base == c.changeset {
            // Rolled back: amend again.
            wc.amend(None).await.unwrap();
        }
        let base = wc.state().base;
        assert_ne!(base, c.changeset);
        let rec = srv.ctx().history.changeset(&base).await.unwrap();
        assert_eq!(rec.changeset.change_id, c.change_id, "{point}");
        assert_eq!(rec.changeset.parents, c.parents, "{point}");
        let st = wc.status().await.unwrap();
        assert!(st.is_clean(), "{point}: {st:?}");
        let fresh = scion_wc::testing::checkout_snapshot(&s, &base.to_text(), &[])
            .await
            .unwrap();
        assert_eq!(fresh, wc_snapshot(&wc).unwrap());
        validate_server(&srv).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn definitive_failure_rolls_back_at_once() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let r = main_ref();
    seed_files(&s, &r, &[("a", Mode::File, b"a\n")], &[])
        .await
        .unwrap();
    let faults = FaultRegistry::new();
    let mut wc = checkout_at(&s, r.as_str(), Some(faults.clone()))
        .await
        .unwrap();
    write_files(&wc, &[("a", Mode::File, b"a2\n")]).unwrap();
    let base = wc.state().base;
    faults.arm(
        points::COMMIT_AFTER_UPLOAD,
        FaultAction::Error(scion_core::ErrorCode::PermissionDenied),
        Some(1),
    );
    let e = wc.commit(b"m", b"A", None).await.unwrap_err();
    assert_eq!(e.code, scion_core::ErrorCode::PermissionDenied);
    assert!(wc.state().pending.is_none());
    assert_eq!(wc.state().base, base);
    assert!(!wc.root().join(".scion").join("commit-request").exists());
    assert_eq!(wc.status().await.unwrap().modified, vec![rp("a")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_commit_journal_rolls_back() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let faults = FaultRegistry::new();
    let mut wc = checkout_at(&s, "main", Some(faults.clone())).await.unwrap();
    write_files(&wc, &[("a", Mode::File, b"a\n")]).unwrap();
    faults.arm(points::COMMIT_AFTER_UPLOAD, FaultAction::Crash, Some(1));
    assert!(wc.commit(b"m", b"A", None).await.is_err());
    faults.reset();
    let root = wc.root().to_path_buf();
    drop(wc);
    std::fs::remove_file(root.join(".scion").join("commit-request")).unwrap();
    let mut wc = WorkingCopy::open(&root, s.client.clone(), WcOptions::default()).unwrap();
    let what = wc.recover().await.unwrap().unwrap();
    assert!(what.contains("rolled back"), "{what}");
    assert!(wc.state().pending.is_none());
    assert_eq!(wc.status().await.unwrap().added, vec![rp("a")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn server_down_keeps_the_journal_for_later() {
    let mut srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let faults = FaultRegistry::new();
    let mut wc = checkout_at(&s, "main", Some(faults.clone())).await.unwrap();
    write_files(&wc, &[("a", Mode::File, b"a\n")]).unwrap();
    faults.arm(points::COMMIT_AFTER_UPLOAD, FaultAction::Crash, Some(1));
    assert!(wc.commit(b"m", b"A", None).await.is_err());
    faults.reset();
    srv.kill().await;
    // Recovery cannot reach the server: the pending commit stays.
    assert!(wc.recover().await.is_err());
    assert!(matches!(
        wc.state().pending,
        Some(PendingOperation::Commit { .. })
    ));
    srv.restart().await;
    let s2 = scenario(&srv, &alice()).await;
    let root = wc.root().to_path_buf();
    drop(wc);
    let mut wc = WorkingCopy::open(&root, s2.client.clone(), WcOptions::default()).unwrap();
    let what = wc.recover().await.unwrap().unwrap();
    assert!(what.contains("completed"), "{what}");
    assert!(wc.status().await.unwrap().is_clean());
    validate_server(&srv).await;
}

/// A two-member local stack on top of an advanced `main`; returns the working copy (with
/// `faults`) and the stack head.
#[allow(clippy::unwrap_used)] // test helper
async fn two_member_stack(
    s: &scion_wc::testing::Scenario,
    faults: &std::sync::Arc<FaultRegistry>,
) -> (WorkingCopy, scion_core::ObjectId) {
    let r = main_ref();
    let start = seed_files(s, &r, &[("a", Mode::File, b"a\n")], &[])
        .await
        .unwrap();
    let mut wc = checkout_at(s, &start.to_text(), Some(faults.clone()))
        .await
        .unwrap();
    seed_files(s, &r, &[("u", Mode::File, b"u\n")], &[])
        .await
        .unwrap();
    write_files(&wc, &[("b", Mode::File, b"b\n")]).unwrap();
    wc.commit(b"m1\n", b"A", None).await.unwrap();
    write_files(&wc, &[("c", Mode::File, b"c\n")]).unwrap();
    let head = wc.commit(b"m2\n", b"A", None).await.unwrap();
    (wc, head)
}

#[tokio::test(flavor = "multi_thread")]
async fn definitive_rebase_failure_returns_to_the_original_at_once() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let faults = FaultRegistry::new();
    let (mut wc, head) = two_member_stack(&s, &faults).await;
    let disk = wc_snapshot(&wc).unwrap();
    faults.arm(
        points::REBASE_MID_MEMBER,
        FaultAction::Error(scion_core::ErrorCode::PermissionDenied),
        Some(1),
    );
    let e = wc.rebase("main").await.unwrap_err();
    faults.reset();
    assert_eq!(e.code, scion_core::ErrorCode::PermissionDenied);
    assert!(wc.state().pending.is_none());
    assert_eq!(wc.state().base, head);
    assert_eq!(wc_snapshot(&wc).unwrap(), disk);
    assert!(wc.status().await.unwrap().is_clean());
    // A later rebase works normally.
    let report = wc.rebase("main").await.unwrap();
    assert_eq!(report.rebased.len(), 2);
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_rolls_back_a_rebase_that_fails_definitively() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let faults = FaultRegistry::new();
    let (mut wc, head) = two_member_stack(&s, &faults).await;
    let disk = wc_snapshot(&wc).unwrap();
    faults.arm(points::REBASE_MID_MEMBER, FaultAction::Crash, Some(1));
    assert!(wc.rebase("main").await.is_err());
    faults.reset();
    assert!(matches!(
        wc.state().pending,
        Some(PendingOperation::Rebase { .. })
    ));
    // The resumed run now fails definitively: recovery rolls back instead of failing every
    // later command.
    faults.arm(
        points::REBASE_MID_MEMBER,
        FaultAction::Error(scion_core::ErrorCode::InvalidArgument),
        Some(1),
    );
    let what = wc.recover().await.unwrap().unwrap();
    faults.reset();
    assert!(what.contains("rolled back"), "{what}");
    assert!(wc.state().pending.is_none());
    assert_eq!(wc.state().base, head);
    assert_eq!(wc_snapshot(&wc).unwrap(), disk);
    assert!(wc.status().await.unwrap().is_clean());
    validate_server(&srv).await;
}
