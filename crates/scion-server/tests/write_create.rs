//! SPEC 19.2 / 19.7 / 19.12 via RPC: CreateChangeset (P26).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod object_support;
mod write_support;

use scion_core::{ErrorCode, ErrorPayload, ObjectId, subcode};
use scion_proto::v1;
use scion_server::testing::InProcessServer;
use std::sync::Arc;
use write_support::*;

#[tokio::test(flavor = "multi_thread")]
async fn creator_is_set_by_the_server() {
    let srv = start().await;
    let r = root(&srv).await;
    let (b, n) = blob(&srv, b"hello\n").await;
    for who in [alice(), bob()] {
        let c = srv.client(Some(&who)).await;
        let resp = c
            .write()
            .create_changeset(create_req(&[r], vec![put_entry("a.txt", &b, n)], 1))
            .await
            .unwrap()
            .into_inner();
        let meta = srv
            .ctx()
            .objects
            .get_meta(&id(&resp.meta_id))
            .await
            .unwrap();
        assert_eq!(meta.creator, who.id_string().into_bytes());
        assert_eq!(meta.author, b"Test Author <test@example.com>".to_vec());
        // The changeset's root tree and meta are the ones reported.
        let cs = srv
            .ctx()
            .history
            .changeset(&id(&resp.changeset_id))
            .await
            .unwrap();
        assert_eq!(cs.changeset.root_tree, id(&resp.root_tree));
        assert_eq!(cs.changeset.meta, id(&resp.meta_id));
        assert_eq!(cs.created_by, who.id_string());
    }
}

/// A client trying to smuggle a `creator` (an unknown field number in `MetaFields`) has no
/// effect: unknown fields are ignored and the server sets the creator.
#[tokio::test(flavor = "multi_thread")]
async fn client_cannot_supply_creator() {
    #[derive(Clone, PartialEq, prost::Message)]
    struct SpoofMeta {
        #[prost(bytes = "vec", tag = "1")]
        author: Vec<u8>,
        #[prost(message, optional, tag = "2")]
        author_time: Option<v1::Timestamp>,
        #[prost(bytes = "vec", tag = "5")]
        message: Vec<u8>,
        #[prost(bytes = "vec", tag = "7")]
        creator: Vec<u8>,
        #[prost(bytes = "vec", tag = "100")]
        creator_hi: Vec<u8>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    struct SpoofReq {
        #[prost(bytes = "vec", repeated, tag = "1")]
        parents: Vec<Vec<u8>>,
        #[prost(message, optional, tag = "3")]
        meta_fields: Option<SpoofMeta>,
        #[prost(bytes = "vec", tag = "4")]
        change_id: Vec<u8>,
        #[prost(bytes = "vec", tag = "100")]
        creator: Vec<u8>,
    }
    use prost::Message;
    let srv = start().await;
    let r = root(&srv).await;
    let spoof = SpoofReq {
        parents: vec![r.as_bytes().to_vec()],
        meta_fields: Some(SpoofMeta {
            author: b"a".to_vec(),
            author_time: Some(v1::Timestamp {
                seconds: 1,
                tz_offset_minutes: 0,
            }),
            message: b"m".to_vec(),
            creator: b"github:mallory".to_vec(),
            creator_hi: b"github:mallory".to_vec(),
        }),
        change_id: cid(9),
        creator: b"github:mallory".to_vec(),
    };
    let channel = srv.client(None).await.channel();
    let mut grpc = tonic::client::Grpc::new(channel);
    grpc.ready().await.unwrap();
    let mut req = tonic::Request::new(bytes::Bytes::from(spoof.encode_to_vec()));
    req.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", srv.token_for(&alice()))
            .parse()
            .unwrap(),
    );
    let resp = grpc
        .unary(
            req,
            "/scion.v1.WriteService/CreateChangeset".parse().unwrap(),
            scion_proto::raw::RawCodec,
        )
        .await
        .unwrap()
        .into_inner();
    let resp = v1::CreateChangesetResponse::decode(resp).unwrap();
    let meta = srv
        .ctx()
        .objects
        .get_meta(&id(&resp.meta_id))
        .await
        .unwrap();
    assert_eq!(meta.creator, b"github:alice".to_vec());
}

#[tokio::test(flavor = "multi_thread")]
async fn identical_requests_return_one_id() {
    let srv = start().await;
    let r = root(&srv).await;
    let (b, n) = blob(&srv, b"same\n").await;
    let c = srv.client(Some(&alice())).await;
    let req = create_req(&[r], vec![put_entry("x/y.txt", &b, n)], 5);
    let first = c
        .write()
        .create_changeset(req.clone())
        .await
        .unwrap()
        .into_inner();
    let second = c
        .write()
        .create_changeset(req.clone())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first, second);
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let c = c.clone();
        let req = req.clone();
        tasks.push(tokio::spawn(async move {
            c.write().create_changeset(req).await.unwrap().into_inner()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), first);
    }
    let rows = srv
        .mem
        .as_ref()
        .unwrap()
        .meta
        .all_changesets()
        .into_iter()
        .filter(|row| row.id == id(&first.changeset_id))
        .count();
    assert_eq!(rows, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn expected_root_tree_is_checked() {
    let srv = start().await;
    let r = root(&srv).await;
    let (b, n) = blob(&srv, b"content\n").await;
    let c = srv.client(Some(&alice())).await;
    let mut req = create_req(&[r], vec![put_entry("f", &b, n)], 1);
    req.expected_root_tree = Some(scion_core::empty_tree_id().as_bytes().to_vec());
    let e = c.write().create_changeset(req.clone()).await.unwrap_err();
    expect_err(
        &e,
        ErrorCode::InvalidArgument,
        Some(subcode::ROOT_TREE_MISMATCH),
    );
    // Nothing was created; the right expectation succeeds.
    let ok = c
        .write()
        .create_changeset(create_req(&[r], vec![put_entry("f", &b, n)], 1))
        .await
        .unwrap()
        .into_inner();
    req.expected_root_tree = Some(ok.root_tree.clone());
    let again = c.write().create_changeset(req).await.unwrap().into_inner();
    assert_eq!(again, ok);
}

#[tokio::test(flavor = "multi_thread")]
async fn import_mode_requires_admin_refs() {
    let srv = start().await;
    let (b, n) = blob(&srv, b"imported\n").await;
    let mut req = create_req(&[], vec![put_entry("README", &b, n)], 77);
    req.import_mode = true;
    // alice writes "/" but lacks admin.refs.
    let alice_c = srv.client(Some(&alice())).await;
    let e = alice_c
        .write()
        .create_changeset(req.clone())
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    // Without import mode a parentless changeset after the root is rejected.
    let mut plain = req.clone();
    plain.import_mode = false;
    let e = alice_c.write().create_changeset(plain).await.unwrap_err();
    expect_err(
        &e,
        ErrorCode::InvalidArgument,
        Some(subcode::PARENTLESS_NOT_ALLOWED),
    );
    // admin has admin.refs and write on "/": a second parentless changeset is allowed.
    let admin_c = srv.client(Some(&admin())).await;
    let created = admin_c
        .write()
        .create_changeset(req.clone())
        .await
        .unwrap()
        .into_inner();
    let cs = srv
        .ctx()
        .history
        .changeset(&id(&created.changeset_id))
        .await
        .unwrap();
    assert!(cs.changeset.parents.is_empty());
    assert_ne!(id(&created.changeset_id), root(&srv).await);
    // A second, different parentless import also works.
    let mut req2 = req;
    req2.change_id = cid(78);
    admin_c.write().create_changeset(req2).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_argument_subcodes() {
    let srv = start().await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let (b, n) = blob(&srv, b"data\n").await;
    let base = commit(&srv, &c, &r, &[("dir/file", b"in dir\n"), ("f", b"f\n")], 1).await;
    let missing = ObjectId::hash_blob(b"never uploaded");

    let mut no_meta = create_req(&[r], vec![put_entry("a", &b, n)], 2);
    no_meta.meta_fields = None;
    let mut no_cid = create_req(&[r], vec![put_entry("a", &b, n)], 2);
    no_cid.change_id = Vec::new();
    let mut short_cid = create_req(&[r], vec![put_entry("a", &b, n)], 2);
    short_cid.change_id = vec![1, 2, 3];
    let mut no_time = create_req(&[r], vec![put_entry("a", &b, n)], 2);
    no_time.meta_fields.as_mut().unwrap().author_time = None;
    let mut bad_parent = create_req(&[r], vec![put_entry("a", &b, n)], 2);
    bad_parent.parents = vec![vec![9; 5]];
    let unknown_parent = ObjectId::new(scion_core::Kind::Changeset, [3; 32]);

    let cases: Vec<(&str, v1::CreateChangesetRequest, &str)> = vec![
        ("no meta_fields", no_meta, subcode::MISSING_FIELD),
        ("no change_id", no_cid, subcode::MISSING_FIELD),
        ("short change_id", short_cid, subcode::BAD_ID_TEXT),
        ("no author_time", no_time, subcode::MISSING_FIELD),
        ("malformed parent", bad_parent, subcode::BAD_ID_TEXT),
        (
            "missing parent",
            create_req(&[unknown_parent], vec![put_entry("a", &b, n)], 2),
            subcode::MISSING_PARENT,
        ),
        (
            "duplicate parent",
            create_req(&[r, r], vec![put_entry("a", &b, n)], 2),
            subcode::DUPLICATE_PARENT,
        ),
        (
            "unsorted delta",
            create_req(&[r], vec![put_entry("b", &b, n), put_entry("a", &b, n)], 2),
            subcode::DELTA_UNSORTED,
        ),
        (
            "invalid path",
            create_req(&[r], vec![put_entry(".scion/x", &b, n)], 2),
            subcode::INVALID_PATH,
        ),
        (
            "missing blob",
            create_req(&[r], vec![put_entry("a", &missing, 3)], 2),
            subcode::MISSING_BLOB,
        ),
        (
            "size mismatch",
            create_req(&[r], vec![put_entry("a", &b, n + 1)], 2),
            subcode::SIZE_MISMATCH,
        ),
        (
            "delete absent",
            create_req(&[base], vec![del_entry("nope")], 2),
            subcode::DELETE_ABSENT,
        ),
        (
            "delete directory",
            create_req(&[base], vec![del_entry("dir")], 2),
            subcode::DELETE_NOT_FILE,
        ),
        (
            "file under file",
            create_req(&[base], vec![put_entry("f/g", &b, n)], 2),
            subcode::FILE_DIRECTORY,
        ),
        (
            "file over directory without deletes",
            create_req(&[base], vec![put_entry("dir", &b, n)], 2),
            subcode::FILE_DIRECTORY,
        ),
        (
            "duplicate delta path",
            create_req(&[r], vec![put_entry("a", &b, n), put_entry("a", &b, n)], 2),
            subcode::DELTA_DUPLICATE,
        ),
    ];
    for (what, req, sub) in cases {
        let e = c.write().create_changeset(req).await.unwrap_err();
        let got = err(&e);
        assert_eq!(got.code, ErrorCode::InvalidArgument, "{what}: {got:?}");
        assert_eq!(got.subcode(), Some(sub), "{what}: {got:?}");
        assert!(!got.retryable(), "{what}");
    }
    // SPEC 4.4 path rules, each rejected with INVALID_ARGUMENT/invalid_path.
    let long_name = "n".repeat(256);
    let long_path = vec!["d".repeat(200); 21].join("/");
    let bad_paths: Vec<Vec<u8>> = vec![
        b".".to_vec(),
        b"..".to_vec(),
        b"a/./b".to_vec(),
        b"a/../b".to_vec(),
        b"/abs".to_vec(),
        b"trailing/".to_vec(),
        b"a//b".to_vec(),
        b"nul\0byte".to_vec(),
        b".scion".to_vec(),
        b"x/.SCION/y".to_vec(),
        b".Scion/z".to_vec(),
        long_name.into_bytes(),
        long_path.into_bytes(),
    ];
    for path in bad_paths {
        let mut entry = put_entry("placeholder", &b, n);
        entry.path = path.clone();
        let e = c
            .write()
            .create_changeset(create_req(&[r], vec![entry], 2))
            .await
            .unwrap_err();
        let got = err(&e);
        assert_eq!(got.code, ErrorCode::InvalidArgument, "{path:?}: {got:?}");
        assert_eq!(
            got.subcode(),
            Some(subcode::INVALID_PATH),
            "{path:?}: {got:?}"
        );
    }
    // A 255-byte name is valid.
    commit(&srv, &c, &r, &[("n".repeat(255).as_str(), b"ok\n")], 4).await;

    // No-op put: the same content at the same path.
    let (fb, fnn) = blob(&srv, b"f\n").await;
    let e = c
        .write()
        .create_changeset(create_req(&[base], vec![put_entry("f", &fb, fnn)], 3))
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::NOOP_PUT));
}

#[tokio::test(flavor = "multi_thread")]
async fn limits_give_too_large() {
    let srv = InProcessServer::start(scion_server::testing::ServerTestOpts {
        tweak: Some(Arc::new(|c| {
            c.limits.max_message_bytes = 8;
            c.limits.max_parents = 1;
            c.limits.max_delta_entries = 2;
        })),
        ..opts()
    })
    .await;
    let r = root(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let (b, n) = blob(&srv, b"x").await;
    let mut long_msg = create_req(&[r], vec![put_entry("a", &b, n)], 1);
    long_msg.meta_fields.as_mut().unwrap().message = vec![b'm'; 9];
    let e = c.write().create_changeset(long_msg).await.unwrap_err();
    let got = expect_err(&e, ErrorCode::TooLarge, None);
    assert_eq!(
        got.payload,
        ErrorPayload::Limit {
            name: "max_message_bytes".into(),
            limit: 8,
            actual: 9
        }
    );
    let other = commit(&srv, &c, &r, &[("o", b"o")], 2).await;
    let e = c
        .write()
        .create_changeset(create_req(&[r, other], vec![put_entry("a", &b, n)], 3))
        .await
        .unwrap_err();
    let got = expect_err(&e, ErrorCode::TooLarge, None);
    assert!(matches!(got.payload, ErrorPayload::Limit { ref name, .. } if name == "max_parents"));
    let e = c
        .write()
        .create_changeset(create_req(
            &[r],
            vec![
                put_entry("a", &b, n),
                put_entry("b", &b, n),
                put_entry("c", &b, n),
            ],
            4,
        ))
        .await
        .unwrap_err();
    let got = expect_err(&e, ErrorCode::TooLarge, None);
    assert!(
        matches!(got.payload, ErrorPayload::Limit { ref name, .. } if name == "max_delta_entries")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn put_of_redacted_blob_is_rejected() {
    let srv = start().await;
    let r = root(&srv).await;
    let (b, n) = blob(&srv, b"leaked secret\n").await;
    srv.ctx()
        .redactor
        .redact(&[b], "leak", &admin(), None)
        .await
        .unwrap();
    let c = srv.client(Some(&alice())).await;
    let e = c
        .write()
        .create_changeset(create_req(&[r], vec![put_entry("s", &b, n)], 1))
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::Redacted, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn unwritable_paths_are_rejected() {
    let srv = InProcessServer::start(opts_with_acl(RESTRICTED_ACL)).await;
    let r = root(&srv).await;
    let (b, n) = blob(&srv, b"k").await;
    let alice_c = srv.client(Some(&alice())).await;
    let e = alice_c
        .write()
        .create_changeset(create_req(
            &[r],
            vec![put_entry("pub", &b, n), put_entry("secrets/key", &b, n)],
            1,
        ))
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    // `/secrets` is component-wise: `secrets2` is governed by "/".
    alice_c
        .write()
        .create_changeset(create_req(&[r], vec![put_entry("secrets2/key", &b, n)], 2))
        .await
        .unwrap();
    // A reader without write is denied everywhere; admin may write the restricted prefix.
    let reader = srv.client(Some(&gh("reader"))).await;
    let e = reader
        .write()
        .create_changeset(create_req(&[r], vec![put_entry("pub", &b, n)], 3))
        .await
        .unwrap_err();
    expect_err(&e, ErrorCode::PermissionDenied, None);
    srv.client(Some(&admin()))
        .await
        .write()
        .create_changeset(create_req(&[r], vec![put_entry("secrets/key", &b, n)], 4))
        .await
        .unwrap();
}

/// SPEC 12.5: a Put may reference only a blob the caller uploaded or can read at some path;
/// otherwise the new `object_paths` row would make an unreadable blob readable at the Put's
/// path. The rejection looks exactly like a missing blob, so it reveals neither existence nor
/// size.
#[tokio::test(flavor = "multi_thread")]
async fn put_cannot_reference_an_unreadable_blob() {
    use object_support::{status_of, upload_blob};
    let srv = InProcessServer::start(opts_with_acl(RESTRICTED_ACL)).await;
    let r = root(&srv).await;
    let secret: &[u8] = b"prod key\n";
    let n = secret.len() as u64;
    let admin_c = srv.client(Some(&admin())).await;
    let (b, _) = upload_blob(&admin_c, secret).await;
    let (p, _) = upload_blob(&admin_c, b"public\n").await;
    admin_c
        .write()
        .create_changeset(create_req(
            &[r],
            vec![put_entry("pub/a", &p, 7), put_entry("secrets/key", &b, n)],
            1,
        ))
        .await
        .unwrap();
    let alice_c = srv.client(Some(&alice())).await;
    assert_eq!(status_of(&alice_c, &b).await, v1::ItemStatus::Denied);

    for (size, change) in [(n, 2), (n + 1, 3)] {
        let e = alice_c
            .write()
            .create_changeset(create_req(
                &[r],
                vec![put_entry("alice/copy", &b, size)],
                change,
            ))
            .await
            .unwrap_err();
        expect_err(&e, ErrorCode::InvalidArgument, Some(subcode::MISSING_BLOB));
    }
    assert_eq!(status_of(&alice_c, &b).await, v1::ItemStatus::Denied);

    // A blob readable at some path may be copied without uploading it.
    alice_c
        .write()
        .create_changeset(create_req(&[r], vec![put_entry("pub/b", &p, 7)], 4))
        .await
        .unwrap();
    // Uploading identical content proves the caller holds it.
    let (up, _) = upload_blob(&alice_c, secret).await;
    assert_eq!(up, b);
    alice_c
        .write()
        .create_changeset(create_req(&[r], vec![put_entry("alice/copy", &b, n)], 5))
        .await
        .unwrap();
}

/// SPEC 19.7: every write and ref RPC rejects a missing or invalid token.
#[tokio::test(flavor = "multi_thread")]
async fn write_and_ref_rpcs_require_authentication() {
    let srv = start().await;
    let r = root(&srv).await;
    for token in [None, Some("v4.public.garbage".to_owned())] {
        let c = srv.client(None).await.with_token(token);
        let unauth = |s: tonic::Status| {
            assert_eq!(s.code(), tonic::Code::Unauthenticated, "{s:?}");
            assert_eq!(err(&s).code, ErrorCode::Unauthenticated);
        };
        unauth(
            c.write()
                .create_changeset(create_req(&[r], vec![], 1))
                .await
                .unwrap_err(),
        );
        unauth(
            c.write()
                .rebase(v1::RebaseRequest {
                    changeset: r.as_bytes().to_vec(),
                    onto: r.as_bytes().to_vec(),
                })
                .await
                .unwrap_err(),
        );
        unauth(
            c.write()
                .merge(v1::MergeRequest {
                    a: r.as_bytes().to_vec(),
                    b: r.as_bytes().to_vec(),
                    meta_fields: Some(meta("m")),
                    change_id: cid(1),
                })
                .await
                .unwrap_err(),
        );
        unauth(
            c.write()
                .land(v1::LandRequest {
                    r#ref: "main".into(),
                    head: r.as_bytes().to_vec(),
                    allow_empty: false,
                })
                .await
                .unwrap_err(),
        );
        unauth(
            c.refs()
                .update_ref(v1::UpdateRefRequest {
                    name: "main".into(),
                    expected_version: 1,
                    new_target: r.as_bytes().to_vec(),
                    allow_non_fast_forward: false,
                })
                .await
                .unwrap_err(),
        );
        unauth(
            c.refs()
                .create_ref(v1::CreateRefRequest {
                    name: "x".into(),
                    target: r.as_bytes().to_vec(),
                    reason: "r".into(),
                })
                .await
                .unwrap_err(),
        );
        unauth(
            c.refs()
                .delete_ref(v1::DeleteRefRequest {
                    name: "main".into(),
                    expected_version: 1,
                    reason: "r".into(),
                })
                .await
                .unwrap_err(),
        );
        unauth(
            c.refs()
                .get_ref_log(v1::GetRefLogRequest {
                    name: "main".into(),
                    from_version: 0,
                    limit: 0,
                })
                .await
                .unwrap_err(),
        );
    }
    // Nothing changed.
    assert_eq!(ref_state(&srv, "main").await, (r, 1));
}
