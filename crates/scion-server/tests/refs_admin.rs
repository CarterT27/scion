//! SPEC 7.6 / 17 / 19.7 / 19.12 via RPC: CreateRef, DeleteRef, GetRefLog (P26).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "write_support/mod.rs"]
mod write_support;

use scion_core::{ErrorCode, ObjectId, subcode};
use scion_proto::{ScionClient, v1};
use scion_server::testing::InProcessServer;
use write_support::*;

async fn create_ref(
    c: &ScionClient,
    name: &str,
    target: &ObjectId,
    reason: &str,
) -> Result<u64, tonic::Status> {
    c.refs()
        .create_ref(v1::CreateRefRequest {
            name: name.into(),
            target: target.as_bytes().to_vec(),
            reason: reason.into(),
        })
        .await
        .map(|r| r.into_inner().version)
}

async fn delete_ref(
    c: &ScionClient,
    name: &str,
    expected: u64,
    reason: &str,
) -> Result<(), tonic::Status> {
    c.refs()
        .delete_ref(v1::DeleteRefRequest {
            name: name.into(),
            expected_version: expected,
            reason: reason.into(),
        })
        .await
        .map(|_| ())
}

async fn ref_log(
    c: &ScionClient,
    name: &str,
    from: u64,
    limit: u32,
) -> Result<Vec<v1::RefLogEntryMsg>, tonic::Status> {
    c.refs()
        .get_ref_log(v1::GetRefLogRequest {
            name: name.into(),
            from_version: from,
            limit,
        })
        .await
        .map(|r| r.into_inner().entries)
}

#[tokio::test(flavor = "multi_thread")]
async fn create_ref_requires_admin_refs_and_is_audited() {
    let srv = start().await;
    let r = root(&srv).await;
    let alice_c = srv.client(Some(&alice())).await;
    let admin_c = srv.client(Some(&admin())).await;

    let e = create_ref(&alice_c, "feature", &r, "want it")
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    assert!(
        srv.ctx()
            .history
            .resolve_ref(&scion_object::RefName::parse("feature").unwrap(), None)
            .await
            .is_err()
    );

    assert_eq!(
        create_ref(&admin_c, "feature", &r, "branch for Q3")
            .await
            .unwrap(),
        1
    );
    assert_eq!(ref_state(&srv, "feature").await, (r, 1));
    let audit = srv.storage.meta.audit_log(100).await.unwrap();
    let rows: Vec<_> = audit
        .iter()
        .filter(|a| a.action == "ref_create" && a.target == "feature")
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].actor, "github:admin");
    assert_eq!(rows[0].reason, "branch for Q3");

    // Existing ref, unknown target, bad arguments.
    let e = create_ref(&admin_c, "feature", &r, "again")
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::REF_EXISTS));
    let unknown = ObjectId::new(scion_core::Kind::Changeset, [8; 32]);
    let e = create_ref(&admin_c, "other", &unknown, "x")
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::NotFound, None);
    let e = create_ref(&admin_c, "", &r, "x").await.unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::MISSING_FIELD));
    let e = create_ref(&admin_c, "bad name", &r, "x").await.unwrap_err();
    expect_err(
        &e,
        ErrorCode::InvalidArgument,
        Some(subcode::INVALID_REF_NAME),
    );
    let e = admin_c
        .refs()
        .create_ref(v1::CreateRefRequest {
            name: "other".into(),
            target: Vec::new(),
            reason: String::new(),
        })
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::MISSING_FIELD));
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_ref_requires_admin_refs_and_is_audited() {
    let srv = start().await;
    let r = root(&srv).await;
    let alice_c = srv.client(Some(&alice())).await;
    let admin_c = srv.client(Some(&admin())).await;
    create_ref(&admin_c, "tmp", &r, "scratch").await.unwrap();
    let x = commit(&srv, &alice_c, &r, &[("x", b"x\n")], 1).await;
    alice_c
        .refs()
        .update_ref(v1::UpdateRefRequest {
            name: "tmp".into(),
            expected_version: 1,
            new_target: x.as_bytes().to_vec(),
            allow_non_fast_forward: false,
        })
        .await
        .unwrap();

    let e = delete_ref(&alice_c, "tmp", 2, "cleanup").await.unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    let e = delete_ref(&admin_c, "tmp", 1, "cleanup").await.unwrap_err();
    let got = expect_err(&e, ErrorCode::RefVersionMismatch, None);
    assert_eq!(ref_mismatch(&got), (2, Some(x)));
    let e = delete_ref(&admin_c, "nope", 1, "cleanup")
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::NotFound, None);
    let e = delete_ref(&admin_c, "", 1, "cleanup").await.unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::MISSING_FIELD));

    delete_ref(&admin_c, "tmp", 2, "done with it")
        .await
        .unwrap();
    assert!(
        srv.ctx()
            .history
            .resolve_ref(&scion_object::RefName::parse("tmp").unwrap(), None)
            .await
            .is_err()
    );
    let audit = srv.storage.meta.audit_log(100).await.unwrap();
    let rows: Vec<_> = audit
        .iter()
        .filter(|a| a.action == "ref_delete" && a.target == "tmp")
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reason, "done with it");
    assert_eq!(rows[0].actor, "github:admin");

    // The log keeps the deletion; re-creating continues the version sequence.
    let log = ref_log(&admin_c, "tmp", 0, 0).await.unwrap();
    assert_eq!(
        log.iter().map(|e| e.version).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(log[2].new_target.is_empty());
    assert_eq!(log[2].old_target, x.as_bytes().to_vec());
    assert_eq!(create_ref(&admin_c, "tmp", &r, "again").await.unwrap(), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_refs_is_required_under_a_custom_policy() {
    let srv = InProcessServer::start(opts_with_acl(RESTRICTED_ACL)).await;
    let r = root(&srv).await;
    // ci may update/force green/* but has no admin.refs.
    let ci_c = srv.client(Some(&ci())).await;
    let e = create_ref(&ci_c, "green/new", &r, "x").await.unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    let e = delete_ref(&ci_c, "main", 1, "x").await.unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    let admin_c = srv.client(Some(&admin())).await;
    create_ref(&admin_c, "green/new", &r, "x").await.unwrap();
    delete_ref(&admin_c, "green/new", 1, "x").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn get_ref_log_returns_entries() {
    let srv = InProcessServer::start(opts_with_acl(RESTRICTED_ACL)).await;
    let r = root(&srv).await;
    let alice_c = srv.client(Some(&alice())).await;
    let mut prev = r;
    let mut targets = vec![r];
    for i in 0..4u32 {
        let next = commit(
            &srv,
            &alice_c,
            &prev,
            &[("f", format!("{i}\n").as_bytes())],
            i,
        )
        .await;
        alice_c
            .refs()
            .update_ref(v1::UpdateRefRequest {
                name: "main".into(),
                expected_version: 1 + u64::from(i),
                new_target: next.as_bytes().to_vec(),
                allow_non_fast_forward: false,
            })
            .await
            .unwrap();
        targets.push(next);
        prev = next;
    }
    let all = ref_log(&alice_c, "main", 0, 0).await.unwrap();
    assert_eq!(all.len(), 5);
    assert_eq!(all[0].version, 1);
    assert!(all[0].old_target.is_empty());
    assert_eq!(all[0].principal, "service:scion");
    for (i, e) in all.iter().enumerate() {
        assert_eq!(e.version, i as u64 + 1);
        assert_eq!(e.new_target, targets[i].as_bytes().to_vec());
        assert!(e.at_unix_ms > 0 || i == 0);
        if i > 0 {
            assert_eq!(e.old_target, targets[i - 1].as_bytes().to_vec());
            assert_eq!(e.principal, "github:alice");
        }
    }
    // from_version and limit.
    let page = ref_log(&alice_c, "main", 3, 2).await.unwrap();
    assert_eq!(
        page.iter().map(|e| e.version).collect::<Vec<_>>(),
        vec![3, 4]
    );
    // A read-only principal may read the log; one without repository read may not.
    assert_eq!(
        ref_log(&srv.client(Some(&gh("reader"))).await, "main", 0, 0)
            .await
            .unwrap()
            .len(),
        5
    );
    let e = ref_log(&srv.client(Some(&mallory())).await, "main", 0, 0)
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    // Unknown names have no entries; bad names are rejected.
    assert!(ref_log(&alice_c, "unknown", 0, 0).await.unwrap().is_empty());
    let e = ref_log(&alice_c, "", 0, 0).await.unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::MISSING_FIELD));
    let e = ref_log(&alice_c, "a..b", 0, 0).await.unwrap_err();
    expect_err(
        &e,
        ErrorCode::InvalidArgument,
        Some(subcode::INVALID_REF_NAME),
    );
}
