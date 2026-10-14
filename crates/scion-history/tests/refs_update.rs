//! SPEC 19.5 (library level): refs, UpdateRef, the ref log and ref administration
//! (SPEC 4.3.6, 4.3.7, 5.3 step 3, 8.5; PLAN 6.7, 6.8).

mod land_support;

use land_support::*;
use scion_core::fault::FaultAction;
use scion_core::{ErrorCode, ErrorPayload, ObjectId, subcode};
use scion_history::UpdateRefRequest;
use std::collections::BTreeSet;

fn upd(expected_version: u64, new_target: ObjectId, force: bool) -> UpdateRefRequest {
    UpdateRefRequest {
        name: main(),
        expected_version,
        new_target,
        allow_non_fast_forward: force,
    }
}

#[tokio::test]
async fn resolve_ref_is_strong_and_serves_historical_versions() {
    let env = env().await;
    let row = env.main_row().await;
    assert_eq!((row.target, row.version), (env.root, 1));

    let a = env.commit(env.root, "a.txt", "a\n", 1).await;
    let up = env
        .h
        .update_ref(upd(1, a, false), &alice(), &Authz::ALL)
        .await
        .unwrap();
    assert_eq!((up.target, up.version), (a, 2));
    assert_eq!(env.main_row().await, up);

    let at1 = env.h.resolve_ref(&main(), Some(1)).await.unwrap();
    assert_eq!((at1.target, at1.version), (env.root, 1));
    let at2 = env.h.resolve_ref(&main(), Some(2)).await.unwrap();
    assert_eq!((at2.target, at2.version), (a, 2));

    let missing = env.h.resolve_ref(&main(), Some(3)).await.unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
    let missing = env.h.resolve_ref(&rn("nope"), None).await.unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
    let missing = env.h.resolve_ref(&rn("nope"), Some(1)).await.unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn aba_cas_expecting_the_first_version_fails() {
    let env = env().await;
    let a = env.root;
    let b = env.commit(a, "b.txt", "b\n", 1).await;
    // A -> B (fast-forward), B -> A (forced).
    env.h
        .update_ref(upd(1, b, false), &alice(), &Authz::ALL)
        .await
        .unwrap();
    let back = env
        .h
        .update_ref(upd(2, a, true), &alice(), &Authz::ALL)
        .await
        .unwrap();
    assert_eq!((back.target, back.version), (a, 3));

    // The target is A again, but a CAS expecting version 1 must fail.
    let c = env.commit(a, "c.txt", "c\n", 2).await;
    let err = env
        .h
        .update_ref(upd(1, c, false), &alice(), &Authz::ALL)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::RefVersionMismatch);
    assert!(err.retryable());
    assert_eq!(
        err.payload,
        ErrorPayload::RefMismatch {
            current_version: 3,
            current_target: Some(a)
        }
    );
    assert_eq!(env.main_row().await.version, 3);
}

#[tokio::test]
async fn fast_forward_enforcement_requires_flag_and_force() {
    let env = env().await;
    let base = env.commit(env.root, "x.txt", "base\n", 1).await;
    env.h
        .update_ref(upd(1, base, false), &alice(), &Authz::ALL)
        .await
        .unwrap();
    // A sibling of `base`: not a descendant of the current target.
    let sibling = env.commit(env.root, "y.txt", "sibling\n", 2).await;

    let no_flag = env
        .h
        .update_ref(upd(2, sibling, false), &alice(), &Authz::ALL)
        .await
        .unwrap_err();
    assert_eq!(no_flag.code, ErrorCode::NotFastForward);

    let no_force = Authz {
        force: false,
        ..Authz::ALL
    };
    let flag_without_force = env
        .h
        .update_ref(upd(2, sibling, true), &alice(), &no_force)
        .await
        .unwrap_err();
    assert_eq!(flag_without_force.code, ErrorCode::NotFastForward);
    assert_eq!(env.main_row().await.target, base);
    assert!(env.h.meta().audit_log(100).await.unwrap().is_empty());

    // Fast-forward needs neither the flag nor force.
    let child = env.commit(base, "z.txt", "child\n", 3).await;
    let ff = env
        .h
        .update_ref(upd(2, child, false), &alice(), &no_force)
        .await
        .unwrap();
    assert_eq!(ff.version, 3);
    assert!(env.h.meta().audit_log(100).await.unwrap().is_empty());

    // Flag + force: the forced update lands and is audited in the same transaction.
    let forced = env
        .h
        .update_ref(upd(3, sibling, true), &alice(), &Authz::ALL)
        .await
        .unwrap();
    assert_eq!((forced.target, forced.version), (sibling, 4));
    let audit = env.h.meta().audit_log(100).await.unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].action, "ref_force_update");
    assert_eq!(audit[0].target, "main");
    assert_eq!(audit[0].actor, "github:alice");
}

#[tokio::test]
async fn update_ref_checks_permission_existence_and_target() {
    let env = env().await;
    let a = env.commit(env.root, "a.txt", "a\n", 1).await;
    let no_update = Authz {
        update: false,
        ..Authz::ALL
    };
    let denied = env
        .h
        .update_ref(upd(1, a, false), &alice(), &no_update)
        .await
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::PermissionDenied);

    let missing_ref = env
        .h
        .update_ref(
            UpdateRefRequest {
                name: rn("release/1"),
                expected_version: 1,
                new_target: a,
                allow_non_fast_forward: false,
            },
            &alice(),
            &Authz::ALL,
        )
        .await
        .unwrap_err();
    assert_eq!(missing_ref.code, ErrorCode::NotFound);

    let ghost = ObjectId::new(scion_core::Kind::Changeset, [9; 32]);
    let missing_target = env
        .h
        .update_ref(upd(1, ghost, false), &alice(), &Authz::ALL)
        .await
        .unwrap_err();
    assert_eq!(missing_target.code, ErrorCode::NotFound);
    assert_eq!(env.main_row().await.version, 1);
    assert_eq!(env.log(&main()).await.len(), 1);
}

#[tokio::test]
async fn create_and_delete_refs_are_audited_and_versions_continue() {
    let env = env().await;
    let r = rn("release/1");
    let row = env
        .h
        .create_ref(&r, &env.root, &alice(), "cut release")
        .await
        .unwrap();
    assert_eq!((row.target, row.version), (env.root, 1));

    let exists = env
        .h
        .create_ref(&r, &env.root, &alice(), "again")
        .await
        .unwrap_err();
    assert_eq!(exists.code, ErrorCode::InvalidArgument);
    assert_eq!(exists.subcode(), Some(subcode::REF_EXISTS));

    let ghost = ObjectId::new(scion_core::Kind::Changeset, [9; 32]);
    let bad = env
        .h
        .create_ref(&rn("green/x"), &ghost, &alice(), "r")
        .await
        .unwrap_err();
    assert_eq!(bad.code, ErrorCode::NotFound);

    let mismatch = env
        .h
        .delete_ref(&r, 5, &alice(), "wrong version")
        .await
        .unwrap_err();
    assert_eq!(mismatch.code, ErrorCode::RefVersionMismatch);
    assert_eq!(
        mismatch.payload,
        ErrorPayload::RefMismatch {
            current_version: 1,
            current_target: Some(env.root)
        }
    );

    env.h.delete_ref(&r, 1, &alice(), "retire").await.unwrap();
    assert_eq!(
        env.h.resolve_ref(&r, None).await.unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(
        env.h
            .delete_ref(&r, 2, &alice(), "gone")
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    // The deletion entry is not a resolvable target.
    assert_eq!(
        env.h.resolve_ref(&r, Some(2)).await.unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(
        env.h.resolve_ref(&r, Some(1)).await.unwrap().target,
        env.root
    );

    // Re-creation continues the numbering (storage-semantics 4.3).
    let again = env
        .h
        .create_ref(&r, &env.root, &alice(), "re-cut")
        .await
        .unwrap();
    assert_eq!(again.version, 3);

    let log = env.log(&r).await;
    let versions: Vec<u64> = log.iter().map(|e| e.version).collect();
    assert_eq!(versions, vec![1, 2, 3]);
    assert_eq!(log[1].new_target, None);
    assert_eq!(log[1].old_target, Some(env.root));

    let audit = env.h.meta().audit_log(100).await.unwrap();
    let actions: Vec<(&str, &str)> = audit
        .iter()
        .map(|a| (a.action.as_str(), a.reason.as_str()))
        .collect();
    assert_eq!(
        actions,
        vec![
            ("ref_create", "cut release"),
            ("ref_delete", "retire"),
            ("ref_create", "re-cut")
        ]
    );
    assert!(audit.iter().all(|a| a.target == "release/1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_update_ref_at_one_version_exactly_one_wins() {
    let env = env().await;
    let n = scion_core::test_support::proptest_cases(100) as u8;
    let n = n.clamp(10, 200);
    let mut targets = Vec::new();
    for i in 0..n {
        targets.push(
            env.commit(env.root, &format!("f{i}.txt"), &format!("{i}\n"), i)
                .await,
        );
    }
    let mut tasks = Vec::new();
    for t in targets {
        let h = env.h.clone();
        tasks.push(tokio::spawn(async move {
            h.update_ref(upd(1, t, false), &alice(), &Authz::ALL).await
        }));
    }
    let mut ok = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(row) => {
                assert_eq!(row.version, 2);
                ok += 1;
            }
            Err(e) => assert_eq!(e.code, ErrorCode::RefVersionMismatch),
        }
    }
    assert_eq!(ok, 1);
    let log = env.log(&main()).await;
    assert_eq!(
        log.iter().map(|e| e.version).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ref_log_is_gap_free_and_chained_under_contention() {
    let env = env().await;
    let workers = 8u8;
    let per_worker = 6u8;
    let mut tasks = Vec::new();
    for w in 0..workers {
        let h = env.h.clone();
        tasks.push(tokio::spawn(async move {
            let mut done = 0u8;
            let mut seq = 0u8;
            while done < per_worker {
                let cur = h.resolve_ref(&main(), None).await.unwrap();
                seq += 1;
                let change = w.wrapping_mul(64).wrapping_add(seq);
                let delta = vec![put(&h, &format!("w{w}/{seq}.txt"), "x\n").await];
                let next = commit_delta(&h, vec![cur.target], delta, cid(change)).await;
                match h
                    .update_ref(upd(cur.version, next, false), &alice(), &Authz::ALL)
                    .await
                {
                    Ok(_) => done += 1,
                    Err(e) => assert_eq!(e.code, ErrorCode::RefVersionMismatch),
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let log = env.log(&main()).await;
    let expected = 1 + u64::from(workers) * u64::from(per_worker);
    assert_eq!(log.len() as u64, expected);
    for (i, e) in log.iter().enumerate() {
        assert_eq!(e.version, i as u64 + 1, "gap-free versions");
        if i > 0 {
            assert_eq!(e.old_target, log[i - 1].new_target, "chained targets");
        }
    }
    let row = env.main_row().await;
    assert_eq!(row.version, expected);
    assert_eq!(Some(row.target), log.last().unwrap().new_target);
    assert_fsck_clean(&env.h).await;
}

#[tokio::test]
async fn failed_cas_writes_no_ref_log_row() {
    let env = env().await;
    let a = env.commit(env.root, "a.txt", "a\n", 1).await;
    env.faults.arm(
        "meta.cas_ref.before",
        FaultAction::Error(ErrorCode::Unavailable),
        Some(1),
    );
    let err = env
        .h
        .update_ref(upd(1, a, false), &alice(), &Authz::ALL)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert_eq!(env.log(&main()).await.len(), 1);
    assert_eq!(env.main_row().await.version, 1);

    // Fail-after: committed (ref and log together), response lost.
    env.faults
        .arm("meta.cas_ref.after", FaultAction::FailAfter, Some(1));
    let err = env
        .h
        .update_ref(upd(1, a, false), &alice(), &Authz::ALL)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    let log = env.log(&main()).await;
    assert_eq!(log.len(), 2);
    assert_eq!(log[1].new_target, Some(a));
    assert_eq!(env.main_row().await.version, 2);
    // The retry observes the committed version.
    let retry = env
        .h
        .update_ref(upd(1, a, false), &alice(), &Authz::ALL)
        .await
        .unwrap_err();
    assert_eq!(retry.code, ErrorCode::RefVersionMismatch);
    let distinct: BTreeSet<u64> = env.log(&main()).await.iter().map(|e| e.version).collect();
    assert_eq!(distinct.len(), 2);
}
