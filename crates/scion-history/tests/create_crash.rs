//! SPEC 19.3 / 16.1 class 4: a crash between the SPEC 5.3 steps of CreateChangeset never
//! yields a visible changeset violating SPEC 4.5, and a retry returns the same id.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod create_support;

use create_support::*;
use scion_core::{ErrorCode, FaultAction, points};
use scion_history::AllowAll;

/// Crash points before the changeset row is committed.
const BEFORE_COMMIT: &[&str] = &[
    points::CREATE_AFTER_VALIDATE,
    points::CREATE_AFTER_TREES,
    points::CREATE_AFTER_META,
    points::CREATE_AFTER_DERIVE,
    "meta.commit_changeset.before",
];

async fn crash_at(point: &str, committed: bool) {
    let e = env().await;
    let c1 = create(&e.h, vec![e.root], vec![put(&e.h, "d/x", "1").await]).await;
    let delta = vec![
        put(&e.h, "d/x", "2").await,
        put(&e.h, "d/y/z", "3").await,
        put(&e.h, "new/file", "4").await,
    ];
    let r = req(vec![c1.changeset_id], delta);
    let faults = e.h.objects().storage().faults.clone();
    let before = e.mem.meta.table_counts().changesets;

    faults.arm(point, FaultAction::Crash, Some(1));
    let err =
        e.h.create_changeset(r.clone(), &alice(), &AllowAll)
            .await
            .unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable, "{point}: {err}");
    assert!(faults.hits(point) >= 1, "{point} was never checked");
    // The process "restarts".
    faults.reset();

    let after_crash = e.mem.meta.table_counts().changesets;
    if committed {
        assert_eq!(after_crash, before + 1, "{point}");
    } else {
        assert_eq!(after_crash, before, "{point}: a changeset became visible");
    }
    assert_fsck_clean(&e.h).await;

    // The client retries: same id as an undisturbed creation would give.
    let retried =
        e.h.create_changeset(r.clone(), &alice(), &AllowAll)
            .await
            .unwrap();
    assert_eq!(e.mem.meta.table_counts().changesets, before + 1);
    assert_fsck_clean(&e.h).await;

    let fresh = env().await;
    let fc1 = create(
        &fresh.h,
        vec![fresh.root],
        vec![put(&fresh.h, "d/x", "1").await],
    )
    .await;
    assert_eq!(fc1, c1);
    let fresh_delta = vec![
        put(&fresh.h, "d/x", "2").await,
        put(&fresh.h, "d/y/z", "3").await,
        put(&fresh.h, "new/file", "4").await,
    ];
    let undisturbed = create(&fresh.h, vec![fc1.changeset_id], fresh_delta).await;
    assert_eq!(retried, undisturbed, "{point}");
}

#[tokio::test]
async fn crash_before_commit_leaves_no_visible_changeset() {
    for point in BEFORE_COMMIT {
        crash_at(point, false).await;
    }
}

#[tokio::test]
async fn crash_after_commit_is_consistent_and_retry_is_idempotent() {
    crash_at(points::CREATE_AFTER_COMMIT, true).await;
    // Fail-after on the commit transaction: committed, response lost.
    crash_at("meta.commit_changeset.after", true).await;
}

#[tokio::test]
async fn injected_errors_at_every_storage_write_leave_no_partial_changeset() {
    for point in [
        "meta.insert_object.before",
        "meta.insert_object.after",
        "blob.put.before",
        points::UPLOAD_AFTER_BYTES,
        points::UPLOAD_AFTER_ROW,
    ] {
        let e = env().await;
        let c1 = create(&e.h, vec![e.root], vec![put(&e.h, "a/b", "1").await]).await;
        let r = req(vec![c1.changeset_id], vec![put(&e.h, "a/c", "2").await]);
        let faults = e.h.objects().storage().faults.clone();
        let before = e.mem.meta.table_counts().changesets;
        faults.arm(point, FaultAction::Error(ErrorCode::Unavailable), Some(1));
        let res = e.h.create_changeset(r.clone(), &alice(), &AllowAll).await;
        faults.disarm_all();
        if res.is_err() {
            assert_eq!(e.mem.meta.table_counts().changesets, before, "{point}");
        }
        assert_fsck_clean(&e.h).await;
        let ok = e.h.create_changeset(r, &alice(), &AllowAll).await.unwrap();
        if let Ok(first) = res {
            assert_eq!(first, ok);
        }
        assert_fsck_clean(&e.h).await;
    }
}
