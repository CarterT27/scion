//! SPEC 19.5 / 19.7 / 19.12 via RPC: UpdateRef (P26).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "write_support/mod.rs"]
mod write_support;

use scion_core::{ErrorCode, ObjectId, subcode, test_support::full_mode};
use scion_proto::{ScionClient, v1};
use scion_server::testing::InProcessServer;
use write_support::*;

async fn update(
    c: &ScionClient,
    name: &str,
    expected: u64,
    target: &ObjectId,
    force: bool,
) -> Result<u64, tonic::Status> {
    c.refs()
        .update_ref(v1::UpdateRefRequest {
            name: name.into(),
            expected_version: expected,
            new_target: target.as_bytes().to_vec(),
            allow_non_fast_forward: force,
        })
        .await
        .map(|r| r.into_inner().version)
}

async fn concurrent_cas(n: usize) {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let target = commit(&srv, &c, &r, &[("cas", b"c\n")], 1).await;
    let mut tasks = Vec::with_capacity(n);
    for _ in 0..n {
        let c = c.clone();
        tasks.push(tokio::spawn(async move {
            update(&c, "main", 1, &target, false).await
        }));
    }
    let mut ok = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(v) => {
                assert_eq!(v, 2);
                ok += 1;
            }
            Err(s) => {
                let e = expect_err(&s, ErrorCode::RefVersionMismatch, None);
                assert!(e.retryable());
                assert_eq!(ref_mismatch(&e), (2, Some(target)));
            }
        }
    }
    assert_eq!(ok, 1, "exactly one CAS succeeds");
    assert_eq!(ref_state(&srv, "main").await, (target, 2));
    // Every loser is a ref CAS failure (SPEC 15.2 `scion_ref_cas_failures_total{ref}`).
    assert_eq!(
        srv.ctx()
            .metrics
            .ref_cas_failures
            .with_label_values(&["main"])
            .get(),
        n as u64 - 1
    );

    // Sequential updates advance by exactly one each.
    let mut prev = target;
    for i in 0..5u32 {
        let next = commit(
            &srv,
            &c,
            &prev,
            &[("seq", format!("{i}\n").as_bytes())],
            10 + i,
        )
        .await;
        let v = update(&c, "main", 2 + u64::from(i), &next, false)
            .await
            .unwrap();
        assert_eq!(v, 3 + u64::from(i));
        prev = next;
    }
    let log = c
        .refs()
        .get_ref_log(v1::GetRefLogRequest {
            name: "main".into(),
            from_version: 0,
            limit: 0,
        })
        .await
        .unwrap()
        .into_inner()
        .entries;
    assert_eq!(
        log.iter().map(|e| e.version).collect::<Vec<_>>(),
        (1..=7).collect::<Vec<u64>>(),
        "the ref log has no gaps"
    );
    for w in log.windows(2) {
        assert_eq!(w[1].old_target, w[0].new_target);
    }
}

/// SPEC 19.5: 1000 concurrent UpdateRef calls at the same expected version.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_update_ref_exactly_one_wins() {
    concurrent_cas(1000).await;
}

/// Spec-sized repetition (SCION_FULL): the 1000-way race repeated.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn full_concurrent_update_ref_repeated() {
    let rounds = if full_mode() { 10 } else { 1 };
    for _ in 0..rounds {
        concurrent_cas(1000).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn aba_cas_expecting_an_old_version_fails() {
    let srv = start().await;
    let r = root(&srv).await;
    let alice_c = srv.client(Some(&alice())).await;
    let admin_c = srv.client(Some(&admin())).await;
    let a = commit(&srv, &alice_c, &r, &[("a", b"a\n")], 1).await;
    let b = commit(&srv, &alice_c, &a, &[("b", b"b\n")], 2).await;
    assert_eq!(update(&alice_c, "main", 1, &a, false).await.unwrap(), 2);
    assert_eq!(update(&alice_c, "main", 2, &b, false).await.unwrap(), 3);
    // Back to A (non-fast-forward: admin with force).
    assert_eq!(update(&admin_c, "main", 3, &a, true).await.unwrap(), 4);
    assert_eq!(ref_state(&srv, "main").await, (a, 4));
    // The target is A again, but a CAS expecting version 2 (the first time at A) fails.
    let next = commit(&srv, &alice_c, &a, &[("c", b"c\n")], 3).await;
    let e = update(&alice_c, "main", 2, &next, false).await.unwrap_err();
    let got = expect_err(&e, ErrorCode::RefVersionMismatch, None);
    assert_eq!(ref_mismatch(&got), (4, Some(a)));
    assert_eq!(ref_state(&srv, "main").await, (a, 4));
}

#[tokio::test(flavor = "multi_thread")]
async fn non_fast_forward_needs_flag_and_force() {
    let srv = start().await;
    let r = root(&srv).await;
    let alice_c = srv.client(Some(&alice())).await;
    let admin_c = srv.client(Some(&admin())).await;
    let a = commit(&srv, &alice_c, &r, &[("a", b"a\n")], 1).await;
    let side = commit(&srv, &alice_c, &r, &[("side", b"s\n")], 2).await;
    update(&alice_c, "main", 1, &a, false).await.unwrap();

    // alice has update but no force: denied with or without the flag.
    for flag in [false, true] {
        let e = update(&alice_c, "main", 2, &side, flag).await.unwrap_err();
        expect_err(&e, ErrorCode::NotFastForward, None);
    }
    // admin has force but must also pass the flag.
    let e = update(&admin_c, "main", 2, &side, false).await.unwrap_err();
    let got = expect_err(&e, ErrorCode::NotFastForward, None);
    assert!(!got.retryable());
    assert_eq!(ref_state(&srv, "main").await, (a, 2));
    assert_eq!(update(&admin_c, "main", 2, &side, true).await.unwrap(), 3);
    // The forced update is audited (SPEC 17).
    let audit = srv.storage.meta.audit_log(100).await.unwrap();
    let forced: Vec<_> = audit
        .iter()
        .filter(|a| a.action == "ref_force_update" && a.target == "main")
        .collect();
    assert_eq!(forced.len(), 1);
    assert_eq!(forced[0].actor, "github:admin");
}

#[tokio::test(flavor = "multi_thread")]
async fn update_and_force_are_enforced_per_ref_pattern() {
    let srv = InProcessServer::start(opts_with_acl(RESTRICTED_ACL)).await;
    let r = root(&srv).await;
    let admin_c = srv.client(Some(&admin())).await;
    let alice_c = srv.client(Some(&alice())).await;
    let ci_c = srv.client(Some(&ci())).await;
    for name in ["release/1", "green/build", "topic"] {
        admin_c
            .refs()
            .create_ref(v1::CreateRefRequest {
                name: name.into(),
                target: r.as_bytes().to_vec(),
                reason: "setup".into(),
            })
            .await
            .unwrap();
    }
    let a = commit(&srv, &alice_c, &r, &[("a", b"a\n")], 1).await;
    let side = commit(&srv, &alice_c, &r, &[("s", b"s\n")], 2).await;

    // release/*: admins only.
    let e = update(&alice_c, "release/1", 1, &a, false)
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    assert_eq!(
        update(&admin_c, "release/1", 1, &a, false).await.unwrap(),
        2
    );
    assert_eq!(
        update(&admin_c, "release/1", 2, &side, true).await.unwrap(),
        3
    );

    // green/*: service:ci only, including force; alice (eng) is denied even though "*"
    // would grant her update (longest pattern wins).
    let e = update(&alice_c, "green/build", 1, &a, false)
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    let e = update(&admin_c, "green/build", 1, &a, false)
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    assert_eq!(update(&ci_c, "green/build", 1, &a, false).await.unwrap(), 2);
    assert_eq!(
        update(&ci_c, "green/build", 2, &side, true).await.unwrap(),
        3
    );

    // main (exact rule): eng updates, nobody forces, not even admin.
    assert_eq!(update(&alice_c, "main", 1, &a, false).await.unwrap(), 2);
    let e = update(&admin_c, "main", 2, &side, true).await.unwrap_err();
    expect_err(&e, ErrorCode::NotFastForward, None);
    let e = update(&ci_c, "main", 2, &side, false).await.unwrap_err();
    expect_err(&e, ErrorCode::NotFastForward, None);

    // topic ("*"): eng updates, admins force.
    assert_eq!(update(&alice_c, "topic", 1, &a, false).await.unwrap(), 2);
    let e = update(&alice_c, "topic", 2, &side, true).await.unwrap_err();
    expect_err(&e, ErrorCode::NotFastForward, None);
    assert_eq!(update(&admin_c, "topic", 2, &side, true).await.unwrap(), 3);

    // A principal without any grant.
    let e = update(&srv.client(Some(&mallory())).await, "topic", 3, &a, false)
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn update_ref_argument_errors() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let a = commit(&srv, &c, &r, &[("a", b"a\n")], 1).await;
    let e = update(&c, "", 1, &a, false).await.unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::MISSING_FIELD));
    let e = update(&c, "/bad", 1, &a, false).await.unwrap_err();
    expect_err(
        &e,
        ErrorCode::InvalidArgument,
        Some(subcode::INVALID_REF_NAME),
    );
    let e = c
        .refs()
        .update_ref(v1::UpdateRefRequest {
            name: "main".into(),
            expected_version: 1,
            new_target: Vec::new(),
            allow_non_fast_forward: false,
        })
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::MISSING_FIELD));
    let e = update(&c, "missing", 1, &a, false).await.unwrap_err();
    expect_err(&e, ErrorCode::NotFound, None);
    let unknown = ObjectId::new(scion_core::Kind::Changeset, [4; 32]);
    let e = update(&c, "main", 1, &unknown, false).await.unwrap_err();
    expect_err(&e, ErrorCode::NotFound, None);
    let not_cs = ObjectId::hash_blob(b"blob");
    let e = update(&c, "main", 1, &not_cs, false).await.unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::INVALID_KIND));
    assert_eq!(ref_state(&srv, "main").await, (r, 1));
}
