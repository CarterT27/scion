//! SPEC 19.5 (library level, fake Rebaser): the SPEC 8.4 / 18.4 Land state machine.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod land_support;

use land_support::*;
use scion_core::fault::{FaultAction, points};
use scion_core::{ErrorCode, ErrorPayload, subcode};
use scion_history::{LandAttemptOutcome, LandRequest};
use scion_store::LandedEntry;
use std::sync::atomic::AtomicU32;

#[tokio::test]
async fn land_onto_moved_main_rebases_and_records() {
    let env = env().await;
    let a = env.commit(env.root, "a.txt", "a\n", 1).await;
    let r1 = env.land(a).await.unwrap();
    // `a` sits directly on the tip: the rebase is the identity.
    assert_eq!(r1.target, a);
    assert_eq!(r1.version, 2);
    assert_eq!(
        r1.landed,
        vec![LandedEntry {
            change_id: cid(1),
            source: a,
            result: a
        }]
    );

    let b = env.commit(env.root, "b.txt", "b\n", 2).await;
    let r2 = env.land(b).await.unwrap();
    assert_eq!(r2.version, 3);
    assert_ne!(r2.target, b);
    assert_eq!(r2.landed.len(), 1);
    assert_eq!(r2.landed[0].source, b);
    assert_eq!(r2.landed[0].result, r2.target);
    assert_eq!(env.read(&r2.target, "a.txt").await.unwrap(), b"a\n");
    assert_eq!(env.read(&r2.target, "b.txt").await.unwrap(), b"b\n");
    let rec = env.h.changeset(&r2.target).await.unwrap();
    assert_eq!(rec.changeset.parents, vec![a]);
    assert_eq!(rec.changeset.change_id, cid(2));

    let row = env.main_row().await;
    assert_eq!((row.target, row.version), (r2.target, 3));
    let log = env.log(&main()).await;
    assert_eq!(log.len(), 3);
    assert_eq!(log[2].old_target, Some(a));
    assert_eq!(log[2].new_target, Some(r2.target));
    assert_eq!(log[2].principal, "github:alice");
    assert_eq!(log[2].landed, r2.landed);
    let landed = env
        .h
        .meta()
        .get_landed(&main(), &cid(2))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (landed.source, landed.result, landed.version),
        (b, r2.target, 3)
    );
    assert_eq!(
        env.observer.take(),
        vec![
            (1, LandAttemptOutcome::Committed),
            (1, LandAttemptOutcome::Committed)
        ]
    );
    assert_fsck_clean(&env.h).await;
}

#[tokio::test]
async fn retry_after_commit_with_lost_response_returns_the_recorded_result() {
    let env = env().await;
    let a = env.commit(env.root, "a.txt", "a\n", 1).await;
    env.land(a).await.unwrap();
    let b = env.commit(env.root, "b.txt", "b\n", 2).await;

    // SPEC 16.1 class 5: the CAS commits, the response is lost.
    env.faults
        .arm("meta.cas_ref.after", FaultAction::FailAfter, Some(1));
    let err = env.land(b).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert_eq!(err.subcode(), Some(subcode::INJECTED));
    let after_crash = env.main_row().await;
    assert_eq!(after_crash.version, 3);

    let retry = env.land(b).await.unwrap();
    assert_eq!(retry.target, after_crash.target);
    assert_eq!(retry.version, 3);
    assert_eq!(
        retry.landed,
        vec![LandedEntry {
            change_id: cid(2),
            source: b,
            result: after_crash.target
        }]
    );
    // No second ref version.
    assert_eq!(env.main_row().await.version, 3);
    assert_eq!(env.log(&main()).await.len(), 3);
    let outcomes = env.observer.take();
    assert_eq!(
        outcomes.last().unwrap(),
        &(1, LandAttemptOutcome::Idempotent)
    );
    assert!(outcomes.contains(&(1, LandAttemptOutcome::Failed(ErrorCode::Unavailable))));
}

#[tokio::test]
async fn crash_at_land_after_commit_checkpoint_is_idempotent_too() {
    let env = env().await;
    let a = env.commit(env.root, "a.txt", "a\n", 1).await;
    env.faults
        .arm(points::LAND_AFTER_COMMIT, FaultAction::Crash, Some(1));
    let err = env.land(a).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    env.faults.reset();
    let retry = env.land(a).await.unwrap();
    assert_eq!((retry.target, retry.version), (a, 2));
    assert_eq!(env.log(&main()).await.len(), 2);
}

#[tokio::test]
async fn failure_at_land_after_rebase_leaves_the_ref_unchanged() {
    let env = env().await;
    let a = env.commit(env.root, "a.txt", "a\n", 1).await;
    env.faults.arm(
        points::LAND_AFTER_REBASE,
        FaultAction::Error(ErrorCode::Unavailable),
        Some(1),
    );
    let err = env.land(a).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert_eq!(env.main_row().await.version, 1);
    assert!(
        env.h
            .meta()
            .get_landed(&main(), &cid(1))
            .await
            .unwrap()
            .is_none()
    );
    assert!(env.faults.hits(points::LAND_AFTER_REBASE) >= 1);
    env.land(a).await.unwrap();
    assert_eq!(env.main_row().await.version, 2);
}

#[tokio::test]
async fn amended_change_after_landing_is_already_landed() {
    let env = env().await;
    let a = env.commit(env.root, "a.txt", "a\n", 1).await;
    let landed = env.land(a).await.unwrap();
    // Same change id, different content: an amended revision.
    let amended = env.commit(env.root, "a.txt", "amended\n", 1).await;
    assert_ne!(amended, a);
    let err = env.land(amended).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::AlreadyLanded);
    assert!(!err.retryable());
    assert_eq!(
        err.payload,
        ErrorPayload::AlreadyLanded {
            source: a,
            result: landed.target,
            version: 2
        }
    );
    assert_eq!(env.main_row().await.version, 2);
    assert_eq!(
        env.observer.take().last().unwrap(),
        &(1, LandAttemptOutcome::Failed(ErrorCode::AlreadyLanded))
    );
}

#[tokio::test]
async fn partially_landed_stack_skips_lower_members_and_lands_upper_ones() {
    let env = env().await;
    // Move main so that stack members get rebased (results differ from sources).
    let x = env.commit(env.root, "x.txt", "x\n", 10).await;
    env.land(x).await.unwrap();

    let s1 = env.commit(env.root, "s1.txt", "s1\n", 11).await;
    let s2 = env.commit(s1, "s2.txt", "s2\n", 12).await;
    let lower = env.land(s1).await.unwrap();
    assert_eq!(lower.landed.len(), 1);
    let r1 = lower.target;
    assert_ne!(r1, s1);

    let upper = env.land(s2).await.unwrap();
    assert_eq!(upper.version, 4);
    assert_eq!(upper.landed.len(), 2);
    assert_eq!(
        upper.landed[0],
        LandedEntry {
            change_id: cid(11),
            source: s1,
            result: r1
        }
    );
    assert_eq!(upper.landed[1].source, s2);
    assert_eq!(upper.landed[1].result, upper.target);
    assert_eq!(
        env.h
            .changeset(&upper.target)
            .await
            .unwrap()
            .changeset
            .parents,
        vec![r1]
    );
    for (path, content) in [("x.txt", "x\n"), ("s1.txt", "s1\n"), ("s2.txt", "s2\n")] {
        assert_eq!(
            env.read(&upper.target, path).await.unwrap(),
            content.as_bytes()
        );
    }
    // Only the newly rebased member is in the committing ref_log entry.
    let log = env.log(&main()).await;
    assert_eq!(log.last().unwrap().landed, vec![upper.landed[1].clone()]);
    // Landing the whole stack again is an idempotent no-op.
    let again = env.land(s2).await.unwrap();
    assert_eq!(again, upper);
    assert_eq!(env.log(&main()).await.len(), 4);
    assert_fsck_clean(&env.h).await;
}

#[tokio::test]
async fn stack_of_several_members_lands_in_one_ref_version() {
    let env = env().await;
    let x = env.commit(env.root, "x.txt", "x\n", 1).await;
    env.land(x).await.unwrap();
    let mut head = env.root;
    for i in 0..5u8 {
        head = env
            .commit(head, &format!("s/{i}.txt"), &format!("{i}\n"), 20 + i)
            .await;
    }
    let r = env.land(head).await.unwrap();
    assert_eq!(r.version, 3);
    let ids: Vec<_> = r.landed.iter().map(|e| e.change_id).collect();
    assert_eq!(ids, (20..25u8).map(cid).collect::<Vec<_>>());
    // Chained results: each result's first parent is the previous result.
    let mut prev = x;
    for e in &r.landed {
        assert_eq!(
            env.h.changeset(&e.result).await.unwrap().changeset.parents,
            vec![prev]
        );
        prev = e.result;
    }
    assert_eq!(prev, r.target);
}

#[tokio::test]
async fn stack_containing_a_merge_is_rejected() {
    let env = env().await;
    let y = env.commit(env.root, "y.txt", "y\n", 20).await;
    let z = env.commit(env.root, "z.txt", "z\n", 21).await;
    let zput = put(&env.h, "z.txt", "z\n").await;
    let m = commit_delta(&env.h, vec![y, z], vec![zput], cid(22)).await;
    let s = env.commit(m, "s.txt", "s\n", 23).await;

    for head in [s, m] {
        let err = env.land(head).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(err.subcode(), Some(subcode::MERGE_IN_STACK));
    }
    assert_eq!(env.main_row().await.version, 1);
    assert!(
        env.h
            .meta()
            .get_landed(&main(), &cid(23))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn merges_already_on_the_tip_use_the_full_ancestry_form() {
    let env = env().await;
    let y = env.commit(env.root, "y.txt", "y\n", 20).await;
    let z = env.commit(env.root, "z.txt", "z\n", 21).await;
    let zput = put(&env.h, "z.txt", "z\n").await;
    let m = commit_delta(&env.h, vec![y, z], vec![zput], cid(22)).await;
    env.h
        .update_ref(
            scion_history::UpdateRefRequest {
                name: main(),
                expected_version: 1,
                new_target: m,
                allow_non_fast_forward: false,
            },
            &alice(),
            &Authz::ALL,
        )
        .await
        .unwrap();
    // `w` sits on `z`, which reached main only through the merge's second parent: the stack is
    // just `w` (z is an ancestor of the tip, though not a first-parent one).
    let w = env.commit(z, "w.txt", "w\n", 23).await;
    let r = env.land(w).await.unwrap();
    assert_eq!(r.landed.len(), 1);
    assert_eq!(r.landed[0].source, w);
    assert!(
        env.h
            .meta()
            .get_landed(&main(), &cid(21))
            .await
            .unwrap()
            .is_none()
    );
    for f in ["y.txt", "z.txt", "w.txt"] {
        assert!(env.read(&r.target, f).await.is_some(), "{f}");
    }
    // A deeper stack over the merged side branch.
    let w2 = env.commit(w, "w2.txt", "w2\n", 24).await;
    let w3 = env.commit(w2, "w3.txt", "w3\n", 25).await;
    let r = env.land(w3).await.unwrap();
    let sources: Vec<_> = r.landed.iter().map(|e| e.source).collect();
    // `w` is recorded (landed earlier from the same source), w2 and w3 are rebased.
    assert_eq!(sources, vec![w, w2, w3]);
}

#[tokio::test]
async fn head_already_on_the_ref_is_an_idempotent_noop() {
    let env = env().await;
    let r = env.land(env.root).await.unwrap();
    assert_eq!((r.target, r.version), (env.root, 1));
    assert!(r.landed.is_empty());
    assert_eq!(
        env.observer.take(),
        vec![(1, LandAttemptOutcome::Idempotent)]
    );
}

#[tokio::test]
async fn land_onto_missing_ref_is_not_found() {
    let env = env().await;
    let a = env.commit(env.root, "a.txt", "a\n", 1).await;
    let err = env.land_on(&rn("release/9"), a, false).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn merge_conflict_fails_the_land() {
    let env = env().await;
    let a = env.commit(env.root, "a.txt", "a\n", 1).await;
    env.land(a).await.unwrap();
    let c = env.commit(env.root, "a.txt", "conflicting\n", 2).await;
    let err = env.land(c).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::MergeConflict);
    assert_eq!(env.main_row().await.version, 2);
    assert_eq!(
        env.observer.take().last().unwrap(),
        &(1, LandAttemptOutcome::Failed(ErrorCode::MergeConflict))
    );
}

#[tokio::test]
async fn empty_change_fails_unless_allow_empty() {
    let env = env().await;
    let a = env.commit(env.root, "a.txt", "a\n", 1).await;
    env.land(a).await.unwrap();
    // Same content under another change id: empty after rebase.
    let dup = env.commit(env.root, "a.txt", "a\n", 2).await;
    let err = env.land(dup).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::EmptyChange);
    assert_eq!(env.main_row().await.version, 2);

    // PLAN 6.14: recorded with result = the current onto changeset, no new changeset.
    let r = env.land_on(&main(), dup, true).await.unwrap();
    assert_eq!((r.target, r.version), (a, 3));
    assert_eq!(
        r.landed,
        vec![LandedEntry {
            change_id: cid(2),
            source: dup,
            result: a
        }]
    );
    // The retry is idempotent.
    let again = env.land_on(&main(), dup, true).await.unwrap();
    assert_eq!(again, r);
    assert_eq!(env.log(&main()).await.len(), 3);

    // An empty member in the middle of a stack is skipped; the rest chains on.
    let s1 = env.commit(env.root, "a.txt", "a\n", 3).await;
    let s2 = env.commit(s1, "b.txt", "b\n", 4).await;
    let r = env.land_on(&main(), s2, true).await.unwrap();
    assert_eq!(r.landed.len(), 2);
    assert_eq!(r.landed[0].result, a);
    assert_eq!(r.landed[1].result, r.target);
    assert_eq!(
        env.h.changeset(&r.target).await.unwrap().changeset.parents,
        vec![a]
    );
}

#[tokio::test]
async fn permissions_are_checked() {
    let env = env().await;
    let a = env.commit(env.root, "a.txt", "a\n", 1).await;
    let req = || LandRequest {
        ref_name: main(),
        head: a,
        allow_empty: false,
    };
    for authz in [
        Authz {
            update: false,
            ..Authz::ALL
        },
        Authz {
            write: false,
            ..Authz::ALL
        },
    ] {
        let err = env
            .h
            .land_with(req(), &alice(), &authz, &env.rebaser())
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied);
    }
    // Force is not needed to land.
    let no_force = Authz {
        force: false,
        ..Authz::ALL
    };
    env.h
        .land_with(req(), &alice(), &no_force, &env.rebaser())
        .await
        .unwrap();
}

#[tokio::test]
async fn authorization_is_rechecked_on_every_attempt() {
    for revoked in [
        Authz {
            write: false,
            ..Authz::ALL
        },
        Authz {
            update: false,
            ..Authz::ALL
        },
    ] {
        let env = env().await;
        let a = env.commit(env.root, "a.txt", "a\n", 1).await;
        let source = FlipSource {
            n: 1,
            first: Authz::ALL,
            then: revoked,
            calls: AtomicU32::new(0),
        };
        // Attempt 1 is authorized but loses the CAS; the policy reload revokes the permission.
        env.contended.force_mismatches(1);
        let err = env
            .h
            .land_with(
                LandRequest {
                    ref_name: main(),
                    head: a,
                    allow_empty: false,
                },
                &alice(),
                &source,
                &env.rebaser(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied);
        assert_eq!(source.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            env.observer.take(),
            vec![
                (1, LandAttemptOutcome::CasMismatch),
                (2, LandAttemptOutcome::Failed(ErrorCode::PermissionDenied))
            ]
        );
        let row = env.main_row().await;
        assert_eq!((row.target, row.version), (env.root, 1));
    }
}

#[tokio::test]
async fn lost_cas_races_are_retried() {
    let env = env().await;
    let a = env.commit(env.root, "a.txt", "a\n", 1).await;
    env.contended.force_mismatches(3);
    let r = env.land(a).await.unwrap();
    assert_eq!(r.version, 2);
    assert_eq!(
        env.observer.take(),
        vec![
            (1, LandAttemptOutcome::CasMismatch),
            (2, LandAttemptOutcome::CasMismatch),
            (3, LandAttemptOutcome::CasMismatch),
            (4, LandAttemptOutcome::Committed)
        ]
    );
}

#[tokio::test]
async fn max_attempts_exhaustion_is_retryable_and_leaves_the_ref_unchanged() {
    let env = env_with(fast_land(4)).await;
    let a = env.commit(env.root, "a.txt", "a\n", 1).await;
    let cas_before = env
        .contended
        .cas_calls
        .load(std::sync::atomic::Ordering::SeqCst);
    env.contended.force_mismatches(u32::MAX);
    let err = env.land(a).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert_eq!(err.subcode(), Some(subcode::LAND_CONTENTION));
    assert!(err.retryable());
    let row = env.main_row().await;
    assert_eq!((row.target, row.version), (env.root, 1));
    assert_eq!(env.log(&main()).await.len(), 1);
    assert!(
        env.h
            .meta()
            .get_landed(&main(), &cid(1))
            .await
            .unwrap()
            .is_none()
    );
    let outcomes = env.observer.take();
    assert_eq!(outcomes.len(), 4);
    assert!(
        outcomes
            .iter()
            .enumerate()
            .all(|(i, o)| *o == (i as u32 + 1, LandAttemptOutcome::CasMismatch))
    );
    assert_eq!(
        env.contended
            .cas_calls
            .load(std::sync::atomic::Ordering::SeqCst)
            - cas_before,
        4
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_lands_of_two_revisions_of_one_change_land_once() {
    let env = env().await;
    let r1 = env.commit(env.root, "one.txt", "1\n", 7).await;
    let r2 = env.commit(env.root, "two.txt", "2\n", 7).await;
    let (h1, h2) = (env.h.clone(), env.h.clone());
    let land = |h: std::sync::Arc<scion_history::History>, head| async move {
        let rebaser = FakeRebaser { h: h.clone() };
        h.land_with(
            LandRequest {
                ref_name: main(),
                head,
                allow_empty: false,
            },
            &alice(),
            &scion_history::AllowAll,
            &rebaser,
        )
        .await
    };
    let (a, b) = tokio::join!(tokio::spawn(land(h1, r1)), tokio::spawn(land(h2, r2)));
    let results = [a.unwrap(), b.unwrap()];
    let ok = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(ok, 1, "{results:?}");
    let err = results.iter().find_map(|r| r.as_ref().err()).unwrap();
    assert_eq!(err.code, ErrorCode::AlreadyLanded);
    assert_eq!(env.main_row().await.version, 2);
}

/// Fast-forward `main` to `target` (UpdateRef at the current version).
async fn ff_main(env: &Env, target: scion_core::ObjectId) {
    let row = env.main_row().await;
    env.h
        .update_ref(
            scion_history::UpdateRefRequest {
                name: main(),
                expected_version: row.version,
                new_target: target,
                allow_non_fast_forward: false,
            },
            &alice(),
            &Authz::ALL,
        )
        .await
        .unwrap();
}

/// A merge of `parents` whose delta (relative to the first parent) puts `files`.
async fn merge_of(
    env: &Env,
    parents: Vec<scion_core::ObjectId>,
    files: &[&str],
    change: u8,
) -> scion_core::ObjectId {
    let mut delta = Vec::new();
    for f in files {
        delta.push(put(&env.h, f, &format!("{f}\n")).await);
    }
    commit_delta(&env.h, parents, delta, cid(change)).await
}

/// SPEC 8.4 / 18.5: with merges on `main`, the stack stops at the first changeset that is an
/// ancestor of the tip by ANY path, including side branches merged through nested merges, while
/// unmerged descendants of a merged side branch stay in the stack.
#[tokio::test]
async fn stack_detection_with_nested_merges_on_main() {
    let env = env().await;
    let root = env.root;
    // main: root -> m1 -> m2 -> m3; side branch s1 -> s2 off m1, plus an unmerged x1 on s2.
    let m1 = env.commit(root, "m1.txt", "m1.txt\n", 1).await;
    let m2 = env.commit(m1, "m2.txt", "m2.txt\n", 2).await;
    let m3 = env.commit(m2, "m3.txt", "m3.txt\n", 3).await;
    let s1 = env.commit(m1, "s1.txt", "s1.txt\n", 4).await;
    let s2 = env.commit(s1, "s2.txt", "s2.txt\n", 5).await;
    let x1 = env.commit(s2, "x1.txt", "x1.txt\n", 6).await;
    // A second side branch q1 -> q2 off root, merged into another side merge n = [q2, k1] which
    // is then merged into main (a nested merge: q1 is not on main's first-parent chain, and
    // neither is n).
    let q1 = env.commit(root, "q1.txt", "q1.txt\n", 7).await;
    let q2 = env.commit(q1, "q2.txt", "q2.txt\n", 8).await;
    let k1 = env.commit(root, "k1.txt", "k1.txt\n", 9).await;
    let n = merge_of(&env, vec![q2, k1], &["k1.txt"], 10).await;
    let big = merge_of(&env, vec![m3, s2], &["s1.txt", "s2.txt"], 11).await;
    let top = merge_of(&env, vec![big, n], &["k1.txt", "q1.txt", "q2.txt"], 12).await;
    let mut tip = top;
    for i in 0..20u8 {
        tip = env
            .commit(tip, &format!("t/{i}.txt"), &format!("{i}\n"), 100 + i)
            .await;
    }
    ff_main(&env, tip).await;

    // Stack on the merged side branch: [h1, h2] (s2 is an ancestor through `big`).
    let h1 = env.commit(s2, "h1.txt", "h1\n", 30).await;
    let h2 = env.commit(h1, "h2.txt", "h2\n", 31).await;
    let r = env.land(h2).await.unwrap();
    let sources: Vec<_> = r.landed.iter().map(|e| e.source).collect();
    assert_eq!(sources, vec![h1, h2]);

    // Stack on the unmerged x1 (child of the merged s2): [x1, y1].
    let y1 = env.commit(x1, "y1.txt", "y1\n", 32).await;
    let r = env.land(y1).await.unwrap();
    let sources: Vec<_> = r.landed.iter().map(|e| e.source).collect();
    assert_eq!(sources, vec![x1, y1]);

    // Stack on q1, reachable from the tip only through two merges: [z1].
    let z1 = env.commit(q1, "z1.txt", "z1\n", 33).await;
    let r = env.land(z1).await.unwrap();
    let sources: Vec<_> = r.landed.iter().map(|e| e.source).collect();
    assert_eq!(sources, vec![z1]);

    // A head that is itself an ancestor through merges is an idempotent no-op.
    let row = env.main_row().await;
    let r = env.land(q2).await.unwrap();
    assert_eq!((r.target, r.version), (row.target, row.version));
    assert!(r.landed.is_empty());
    for f in [
        "s1.txt", "s2.txt", "x1.txt", "y1.txt", "q1.txt", "z1.txt", "h2.txt",
    ] {
        assert!(env.read(&row.target, f).await.is_some(), "{f}");
    }
    assert_fsck_clean(&env.h).await;
}

/// SPEC 8.4 `Retrying` on a `landed` unique violation: a concurrent land of another revision
/// commits between `Checking` and `Committing`; the retry re-reads the index and fails
/// ALREADY_LANDED with the ref unchanged.
#[tokio::test]
async fn landed_unique_violation_is_retried_then_already_landed() {
    let env = env().await;
    let x = env.commit(env.root, "x.txt", "x\n", 10).await;
    env.land(x).await.unwrap();
    let a = env.commit(env.root, "a.txt", "a\n", 1).await;
    let landed = env.land(a).await.unwrap();
    env.observer.take();
    // Another revision of change 1 (non-conflicting content).
    let amended = env.commit(env.root, "a2.txt", "a2\n", 1).await;
    env.contended.hide_landed(1);
    let err = env.land(amended).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::AlreadyLanded);
    assert_eq!(
        err.payload,
        ErrorPayload::AlreadyLanded {
            source: a,
            result: landed.target,
            version: landed.version
        }
    );
    assert_eq!(
        env.observer.take(),
        vec![
            (1, LandAttemptOutcome::LandedConflict),
            (2, LandAttemptOutcome::Failed(ErrorCode::AlreadyLanded))
        ]
    );
    let row = env.main_row().await;
    assert_eq!((row.target, row.version), (landed.target, landed.version));
    assert_eq!(env.log(&main()).await.len() as u64, landed.version);
}

/// A `landed` unique violation caused by a concurrent land of the SAME source: the retry finds
/// the recorded row and returns it (idempotent), without a second ref version.
#[tokio::test]
async fn landed_unique_violation_from_the_same_source_is_idempotent() {
    let env = env().await;
    let x = env.commit(env.root, "x.txt", "x\n", 10).await;
    env.land(x).await.unwrap();
    let b = env.commit(env.root, "b.txt", "b\n", 2).await;
    let first = env.land(b).await.unwrap();
    env.observer.take();
    env.contended.hide_landed(1);
    // The blind attempt re-rebases `b` (empty on the new tip, recorded as onto with
    // allow_empty) and loses on the unique index; the retry sees the recorded row.
    let r = env.land_on(&main(), b, true).await.unwrap();
    assert_eq!(r, first);
    assert_eq!(
        env.observer.take(),
        vec![
            (1, LandAttemptOutcome::LandedConflict),
            (2, LandAttemptOutcome::Idempotent)
        ]
    );
    assert_eq!(env.main_row().await.version, first.version);
    assert_eq!(env.log(&main()).await.len() as u64, first.version);
}

/// Two revisions of one change id in a single stack are rejected up front (they could never
/// both be recorded in the `landed` index).
#[tokio::test]
async fn duplicate_change_id_in_one_stack_is_rejected() {
    let env = env().await;
    let s1 = env.commit(env.root, "s1.txt", "s1\n", 40).await;
    let s2 = env.commit(s1, "s2.txt", "s2\n", 40).await;
    let err = env.land(s2).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert_eq!(err.subcode(), Some(subcode::DUPLICATE));
    assert_eq!(
        env.observer.take(),
        vec![(1, LandAttemptOutcome::Failed(ErrorCode::InvalidArgument))]
    );
    assert_eq!(env.main_row().await.version, 1);
}
