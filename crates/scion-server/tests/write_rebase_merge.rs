//! SPEC 19.4 / 19.7 via RPC: Rebase and Merge (P26).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod write_support;

use scion_core::{ConflictKind, ErrorCode, ErrorPayload, ObjectId, subcode};
use scion_proto::{ScionClient, v1};
use scion_server::testing::InProcessServer;
use write_support::*;

async fn rebase(
    c: &ScionClient,
    cs: &ObjectId,
    onto: &ObjectId,
) -> Result<ObjectId, tonic::Status> {
    c.write()
        .rebase(v1::RebaseRequest {
            changeset: cs.as_bytes().to_vec(),
            onto: onto.as_bytes().to_vec(),
        })
        .await
        .map(|r| id(&r.into_inner().changeset_id))
}

async fn merge(
    c: &ScionClient,
    a: &ObjectId,
    b: &ObjectId,
    change_id: u32,
) -> Result<ObjectId, tonic::Status> {
    c.write()
        .merge(v1::MergeRequest {
            a: a.as_bytes().to_vec(),
            b: b.as_bytes().to_vec(),
            meta_fields: Some(meta("merge")),
            change_id: cid(change_id),
        })
        .await
        .map(|r| id(&r.into_inner().changeset_id))
}

#[tokio::test(flavor = "multi_thread")]
async fn rebase_is_deterministic_across_restart() {
    let mut srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let mine = commit(&srv, &c, &r, &[("a.txt", b"a\n")], 1).await;
    let onto = commit(&srv, &c, &r, &[("b.txt", b"b\n")], 2).await;
    let first = rebase(&c, &mine, &onto).await.unwrap();
    assert_ne!(first, mine);
    let rec = srv.ctx().history.changeset(&first).await.unwrap();
    assert_eq!(rec.changeset.parents, vec![onto]);
    let orig = srv.ctx().history.changeset(&mine).await.unwrap();
    assert_eq!(rec.changeset.meta, orig.changeset.meta);
    assert_eq!(rec.changeset.change_id, orig.changeset.change_id);
    // Any reader may rebase someone else's changeset; the result is the same.
    let bob_c = srv.client(Some(&bob())).await;
    assert_eq!(rebase(&bob_c, &mine, &onto).await.unwrap(), first);
    // Fast path: onto == parent returns the changeset itself.
    assert_eq!(rebase(&c, &mine, &r).await.unwrap(), mine);

    srv.restart().await;
    let c = srv.client(Some(&alice())).await;
    assert_eq!(rebase(&c, &mine, &onto).await.unwrap(), first);
}

#[tokio::test(flavor = "multi_thread")]
async fn rebase_conflict_payload_has_clean_entries() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let p = commit(&srv, &c, &r, &[("f", b"base\n")], 1).await;
    let mine = commit(&srv, &c, &p, &[("f", b"mine\n"), ("g", b"new\n")], 2).await;
    let onto = commit(&srv, &c, &p, &[("f", b"theirs\n")], 3).await;
    let e = rebase(&c, &mine, &onto).await.unwrap_err();
    let got = expect_err(&e, ErrorCode::MergeConflict, None);
    assert!(!got.retryable());
    let ErrorPayload::MergeConflict(report) = got.payload else {
        panic!("expected a MergeConflict payload: {got:?}");
    };
    assert_eq!(report.conflicts.len(), 1);
    let conflict = &report.conflicts[0];
    assert_eq!(conflict.path, b"f".to_vec());
    assert_eq!(conflict.kind, ConflictKind::Content);
    let (base_id, _) = blob(&srv, b"base\n").await;
    let (ours_id, _) = blob(&srv, b"theirs\n").await;
    let (theirs_id, _) = blob(&srv, b"mine\n").await;
    assert_eq!(conflict.base.unwrap().id, base_id);
    assert_eq!(conflict.ours.unwrap().id, ours_id);
    assert_eq!(conflict.theirs.unwrap().id, theirs_id);
    assert!(!conflict.hunks.is_empty());
    assert_eq!(report.clean_entries.len(), 1);
    assert_eq!(report.clean_entries[0].path, b"g".to_vec());
    let (g_id, _) = blob(&srv, b"new\n").await;
    assert_eq!(report.clean_entries[0].entry.unwrap().id, g_id);
}

#[tokio::test(flavor = "multi_thread")]
async fn rebase_errors() {
    let srv = InProcessServer::start(opts_with_acl(RESTRICTED_ACL)).await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    // EMPTY_CHANGE: the same content is already in onto.
    let mine = commit(&srv, &c, &r, &[("same", b"x\n")], 1).await;
    let onto = commit(&srv, &c, &r, &[("same", b"x\n")], 2).await;
    let e = rebase(&c, &mine, &onto).await.unwrap_err();
    expect_err(&e, ErrorCode::EmptyChange, None);
    // multi_parent.
    let other = commit(&srv, &c, &r, &[("other", b"o\n")], 3).await;
    let m = merge(&c, &mine, &other, 4).await.unwrap();
    let e = rebase(&c, &m, &onto).await.unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::MULTI_PARENT));
    // NOT_FOUND.
    let unknown = ObjectId::new(scion_core::Kind::Changeset, [5; 32]);
    let e = rebase(&c, &unknown, &onto).await.unwrap_err();
    expect_err(&e, ErrorCode::NotFound, None);
    // Missing and malformed fields.
    let e = c
        .write()
        .rebase(v1::RebaseRequest {
            changeset: Vec::new(),
            onto: onto.as_bytes().to_vec(),
        })
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::MISSING_FIELD));
    let e = c
        .write()
        .rebase(v1::RebaseRequest {
            changeset: mine.as_bytes().to_vec(),
            onto: vec![1, 2, 3],
        })
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::BAD_ID_TEXT));
    // Repository read is required (and sufficient: a read-only principal may rebase).
    let e = rebase(&srv.client(Some(&mallory())).await, &mine, &other)
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    rebase(&srv.client(Some(&gh("reader"))).await, &mine, &other)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_success_sets_parents_and_creator() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let a = commit(&srv, &c, &r, &[("a", b"a\n")], 1).await;
    let b = commit(&srv, &c, &r, &[("b", b"b\n")], 2).await;
    let bob_c = srv.client(Some(&bob())).await;
    let m = merge(&bob_c, &a, &b, 3).await.unwrap();
    let rec = srv.ctx().history.changeset(&m).await.unwrap();
    assert_eq!(rec.changeset.parents, vec![a, b]);
    let meta = srv
        .ctx()
        .objects
        .get_meta(&rec.changeset.meta)
        .await
        .unwrap();
    assert_eq!(meta.creator, b"github:bob".to_vec());
    // The merged tree has both files.
    let h = &srv.ctx().history;
    for p in ["a", "b"] {
        let path = scion_object::RepoPath::parse(p.as_bytes()).unwrap();
        assert!(
            h.lookup(&rec.changeset.root_tree, &path)
                .await
                .unwrap()
                .is_some()
        );
    }
    // Deterministic: the same request returns the same id.
    assert_eq!(merge(&bob_c, &a, &b, 3).await.unwrap(), m);
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_errors() {
    let srv = InProcessServer::start(opts_with_acl(RESTRICTED_ACL)).await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    // MERGE_CONFLICT (add/add).
    let a = commit(&srv, &c, &r, &[("f", b"a\n"), ("only_a", b"1\n")], 1).await;
    let b = commit(&srv, &c, &r, &[("f", b"b\n")], 2).await;
    let e = merge(&c, &a, &b, 3).await.unwrap_err();
    let got = expect_err(&e, ErrorCode::MergeConflict, None);
    let ErrorPayload::MergeConflict(report) = got.payload else {
        panic!("expected MergeConflict payload");
    };
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].kind, ConflictKind::AddAdd);
    assert!(report.conflicts[0].base.is_none());
    assert!(
        report
            .clean_entries
            .iter()
            .any(|ce| ce.path == b"only_a".to_vec())
    );

    // AMBIGUOUS_MERGE_BASE: criss-cross.
    let x = commit(&srv, &c, &r, &[("x", b"x\n")], 10).await;
    let y = commit(&srv, &c, &r, &[("y", b"y\n")], 11).await;
    let m1 = merge(&c, &x, &y, 12).await.unwrap();
    let m2 = merge(&c, &y, &x, 13).await.unwrap();
    let e = merge(&c, &m1, &m2, 14).await.unwrap_err();
    expect_err(&e, ErrorCode::AmbiguousMergeBase, None);

    // PERMISSION_DENIED: the merge result would write a restricted path.
    let admin_c = srv.client(Some(&admin())).await;
    let s = commit(&srv, &admin_c, &r, &[("secrets/k", b"k\n")], 20).await;
    let e = merge(&c, &x, &s, 21).await.unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    // admin may perform the same merge.
    merge(&admin_c, &x, &s, 21).await.unwrap();
    // No repository read at all.
    let e = merge(&srv.client(Some(&mallory())).await, &x, &y, 22)
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);

    // NOT_FOUND and missing fields.
    let unknown = ObjectId::new(scion_core::Kind::Changeset, [6; 32]);
    let e = merge(&c, &x, &unknown, 23).await.unwrap_err();
    expect_err(&e, ErrorCode::NotFound, None);
    for (what, req) in [
        (
            "a",
            v1::MergeRequest {
                a: Vec::new(),
                b: y.as_bytes().to_vec(),
                meta_fields: Some(meta("m")),
                change_id: cid(1),
            },
        ),
        (
            "meta_fields",
            v1::MergeRequest {
                a: x.as_bytes().to_vec(),
                b: y.as_bytes().to_vec(),
                meta_fields: None,
                change_id: cid(1),
            },
        ),
        (
            "change_id",
            v1::MergeRequest {
                a: x.as_bytes().to_vec(),
                b: y.as_bytes().to_vec(),
                meta_fields: Some(meta("m")),
                change_id: Vec::new(),
            },
        ),
    ] {
        let e = c.write().merge(req).await.unwrap_err();
        let got = err(&e);
        assert_eq!(got.code, ErrorCode::InvalidArgument, "{what}");
        assert_eq!(got.subcode(), Some(subcode::MISSING_FIELD), "{what}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_message_limit_is_too_large() {
    let srv = InProcessServer::start(scion_server::testing::ServerTestOpts {
        tweak: Some(std::sync::Arc::new(|c| c.limits.max_message_bytes = 4)),
        ..opts()
    })
    .await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let mut a_req = create_req(&[r], vec![], 1);
    let (b1, n1) = blob(&srv, b"1").await;
    a_req.delta = vec![put_entry("a", &b1, n1)];
    a_req.meta_fields.as_mut().unwrap().message = b"m".to_vec();
    let a = id(&c
        .write()
        .create_changeset(a_req)
        .await
        .unwrap()
        .into_inner()
        .changeset_id);
    let mut b_req = create_req(&[r], vec![put_entry("b", &b1, n1)], 2);
    b_req.meta_fields.as_mut().unwrap().message = b"m".to_vec();
    let b = id(&c
        .write()
        .create_changeset(b_req)
        .await
        .unwrap()
        .into_inner()
        .changeset_id);
    let e = merge(&c, &a, &b, 3).await.unwrap_err();
    expect_err(&e, ErrorCode::TooLarge, None);
}
