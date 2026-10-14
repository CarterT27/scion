//! FakeUpstream non-object services and request recording (PLAN 2.21): ResolveRef,
//! GetChangesets, Resolve, CheckAccess, WhoAmI, ListRedactions; UNIMPLEMENTED for everything
//! else; every request is recorded with method, ids and principal.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use scion_core::{ErrorCode, ErrorPayload, Kind, ObjectId, subcode};
use scion_proto::{ScionClient, error_from_status, v1};
use scion_testkit::{FakeUpstream, FakeUpstreamOpts, RecordedRequest};
use std::sync::Arc;

fn opts() -> FakeUpstreamOpts {
    FakeUpstreamOpts {
        tls: false,
        principal_of: Arc::new(|t| t.strip_prefix("tok-").map(|p| format!("service:{p}"))),
        public_keys: Vec::new(),
    }
}

async fn client(up: &FakeUpstream, who: Option<&str>) -> ScionClient {
    ScionClient::connect(&up.endpoint(), up.tls(), who.map(|w| format!("tok-{w}")))
        .await
        .unwrap()
}

fn cs(n: u8) -> ObjectId {
    ObjectId::new(Kind::Changeset, [n; 32])
}

fn code(st: &tonic::Status) -> ErrorCode {
    error_from_status(st).code
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_are_recorded() {
    let up = FakeUpstream::start(opts()).await;
    let a = ObjectId::hash_blob(b"a");
    let b = ObjectId::hash_blob(b"b");
    up.insert_object(a, Bytes::from_static(b"a"), false);
    let c = client(&up, Some("ci")).await;
    let anon = client(&up, None).await;

    anon.objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap();
    let mut s = c
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: vec![a.as_bytes().to_vec(), b.as_bytes().to_vec()],
            offset: None,
            length: None,
        })
        .await
        .unwrap()
        .into_inner();
    while s.message().await.unwrap().is_some() {}
    c.objects()
        .has_objects(v1::HasObjectsRequest {
            ids: vec![b.as_bytes().to_vec()],
        })
        .await
        .unwrap();
    // Rejected (unauthenticated) calls are recorded too.
    let st = anon
        .objects()
        .has_objects(v1::HasObjectsRequest {
            ids: vec![a.as_bytes().to_vec()],
        })
        .await
        .unwrap_err();
    assert_eq!(code(&st), ErrorCode::Unauthenticated);

    assert_eq!(
        up.requests(),
        vec![
            RecordedRequest {
                method: "/scion.v1.ObjectService/GetServerInfo".into(),
                ids: vec![],
                principal: None,
            },
            RecordedRequest {
                method: "/scion.v1.ObjectService/GetObjects".into(),
                ids: vec![a, b],
                principal: Some("service:ci".into()),
            },
            RecordedRequest {
                method: "/scion.v1.ObjectService/HasObjects".into(),
                ids: vec![b],
                principal: Some("service:ci".into()),
            },
            RecordedRequest {
                method: "/scion.v1.ObjectService/HasObjects".into(),
                ids: vec![a],
                principal: None,
            },
        ]
    );
    assert_eq!(
        up.requests_for("/scion.v1.ObjectService/HasObjects").len(),
        2
    );
    up.reset_requests();
    assert!(up.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn refs_changesets_and_resolve() {
    let up = FakeUpstream::start(opts()).await;
    let c = client(&up, Some("ci")).await;
    up.set_ref("main", cs(1), 1);
    up.set_ref("main", cs(2), 2);
    up.insert_changeset(cs(1), Bytes::from_static(b"enc-1"));
    up.insert_changeset(cs(2), Bytes::from_static(b"enc-2"));

    // ResolveRef: latest, at a version, missing version, missing ref.
    let r = c
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: "main".into(),
            at_version: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!((r.target, r.version), (cs(2).as_bytes().to_vec(), 2));
    let r = c
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: "main".into(),
            at_version: Some(1),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!((r.target, r.version), (cs(1).as_bytes().to_vec(), 1));
    for (name, at) in [("main", Some(7)), ("nope", None)] {
        let st = c
            .objects()
            .resolve_ref(v1::ResolveRefRequest {
                name: name.into(),
                at_version: at,
            })
            .await
            .unwrap_err();
        assert_eq!(code(&st), ErrorCode::NotFound);
    }

    // GetChangesets: per-item ok / not_found / denied without failing the batch.
    up.deny("service:ci", cs(2));
    let items = c
        .query()
        .get_changesets(v1::GetChangesetsRequest {
            ids: vec![
                cs(1).as_bytes().to_vec(),
                cs(9).as_bytes().to_vec(),
                cs(2).as_bytes().to_vec(),
            ],
        })
        .await
        .unwrap()
        .into_inner()
        .items;
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].status, v1::ItemStatus::Ok as i32);
    assert_eq!(
        items[0].view,
        Some(v1::changeset_item::View::Encoded(b"enc-1".to_vec()))
    );
    assert_eq!(items[1].status, v1::ItemStatus::NotFound as i32);
    assert_eq!(items[2].status, v1::ItemStatus::Denied as i32);
    assert!(items[2].view.is_none());

    // Resolve: ref name, full id text, unique prefix, ambiguous prefix, unknown.
    let resolve = |rev: &str| {
        let c = c.clone();
        let rev = rev.to_owned();
        async move {
            c.query()
                .resolve(v1::ResolveRequest { revision: rev })
                .await
                .map(|r| ObjectId::from_bytes(&r.into_inner().changeset).unwrap())
        }
    };
    assert_eq!(resolve("main").await.unwrap(), cs(2));
    assert_eq!(resolve(&cs(1).to_text()).await.unwrap(), cs(1));
    assert_eq!(resolve(&cs(1).hex()[..10]).await.unwrap(), cs(1));
    assert_eq!(
        resolve(&format!("c1{}", &cs(2).hex()[..12])).await.unwrap(),
        cs(2)
    );
    let other = ObjectId::new(Kind::Changeset, {
        let mut d = [1u8; 32];
        d[31] = 2;
        d
    });
    up.insert_changeset(other, Bytes::from_static(b"enc-x"));
    let st = resolve(&cs(1).hex()[..12]).await.unwrap_err();
    let e = error_from_status(&st);
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert_eq!(e.subcode(), Some(subcode::AMBIGUOUS_PREFIX));
    match e.payload {
        ErrorPayload::Candidates(mut v) => {
            v.sort();
            assert_eq!(v, vec![cs(1), other]);
        }
        p => panic!("{p:?}"),
    }
    assert_eq!(
        code(&resolve("deadbeefdead").await.unwrap_err()),
        ErrorCode::NotFound
    );
    assert_eq!(
        code(&resolve("main~1").await.unwrap_err()),
        ErrorCode::InvalidArgument
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_and_admin() {
    let up = FakeUpstream::start(opts()).await;
    let daemon = client(&up, Some("daemon")).await;
    let root_obj = ObjectId::hash_blob(b"root");
    let restricted = ObjectId::hash_blob(b"restricted");
    let denied = ObjectId::hash_blob(b"denied");
    let redacted = ObjectId::hash_blob(b"redacted");
    let missing = ObjectId::hash_blob(b"missing");
    up.insert_object(root_obj, Bytes::from_static(b"root"), false);
    up.insert_object(restricted, Bytes::from_static(b"restricted"), true);
    up.insert_object(denied, Bytes::from_static(b"denied"), true);
    up.insert_object(redacted, Bytes::from_static(b"redacted"), false);
    up.deny("service:alice", denied);
    up.redact(redacted);

    // WhoAmI.
    let who = daemon
        .auth()
        .who_am_i(v1::WhoAmIRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(who.principal, "service:daemon");

    // CheckAccess answers for the subject token, not the caller.
    let ids = [root_obj, restricted, denied, redacted, missing];
    let r = daemon
        .auth()
        .check_access(v1::CheckAccessRequest {
            subject_token: "tok-alice".into(),
            object_ids: ids.iter().map(|i| i.as_bytes().to_vec()).collect(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.allowed, vec![true, true, false, false, false]);
    assert_eq!(
        r.access_class,
        vec![
            v1::AccessClass::Root as i32,
            v1::AccessClass::Restricted as i32,
            v1::AccessClass::Restricted as i32,
            v1::AccessClass::Unspecified as i32,
            v1::AccessClass::Unspecified as i32,
        ]
    );
    let r = daemon
        .auth()
        .check_access(v1::CheckAccessRequest {
            subject_token: "tok-bob".into(),
            object_ids: vec![denied.as_bytes().to_vec()],
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.allowed, vec![true]);
    let st = daemon
        .auth()
        .check_access(v1::CheckAccessRequest {
            subject_token: "garbage".into(),
            object_ids: vec![],
        })
        .await
        .unwrap_err();
    assert_eq!(code(&st), ErrorCode::Unauthenticated);

    // ListRedactions: entries after since_epoch, current epoch.
    let second = ObjectId::hash_blob(b"second");
    up.redact(second);
    let l = daemon
        .admin()
        .list_redactions(v1::ListRedactionsRequest { since_epoch: 0 })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(l.current_epoch, 2);
    let listed: Vec<(Vec<u8>, u64)> = l
        .entries
        .iter()
        .map(|e| (e.object_id.clone(), e.epoch))
        .collect();
    assert_eq!(
        listed,
        vec![
            (redacted.as_bytes().to_vec(), 1),
            (second.as_bytes().to_vec(), 2)
        ]
    );
    let l = daemon
        .admin()
        .list_redactions(v1::ListRedactionsRequest { since_epoch: 1 })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(l.entries.len(), 1);
    assert_eq!(daemon.last_epochs().unwrap().redaction, 2);

    // Unauthenticated ListRedactions / WhoAmI.
    let anon = daemon.with_token(None);
    assert_eq!(
        code(
            &anon
                .admin()
                .list_redactions(v1::ListRedactionsRequest { since_epoch: 0 })
                .await
                .unwrap_err()
        ),
        ErrorCode::Unauthenticated
    );
    assert_eq!(
        code(
            &anon
                .auth()
                .who_am_i(v1::WhoAmIRequest {})
                .await
                .unwrap_err()
        ),
        ErrorCode::Unauthenticated
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn everything_else_is_unimplemented() {
    let up = FakeUpstream::start(opts()).await;
    let c = client(&up, Some("ci")).await;
    let unimpl = |r: Result<(), tonic::Status>| {
        assert_eq!(r.unwrap_err().code(), tonic::Code::Unimplemented);
    };
    unimpl(
        c.query()
            .list_tree(v1::ListTreeRequest::default())
            .await
            .map(|_| ()),
    );
    unimpl(c.query().log(v1::LogRequest::default()).await.map(|_| ()));
    unimpl(
        c.query()
            .read_path(v1::ReadPathRequest::default())
            .await
            .map(|_| ()),
    );
    unimpl(c.query().diff(v1::DiffRequest::default()).await.map(|_| ()));
    unimpl(
        c.auth()
            .login(v1::LoginRequest::default())
            .await
            .map(|_| ()),
    );
    unimpl(
        c.admin()
            .redact(v1::RedactRequest::default())
            .await
            .map(|_| ()),
    );
    unimpl(
        c.write()
            .create_changeset(v1::CreateChangesetRequest::default())
            .await
            .map(|_| ()),
    );
    unimpl(
        c.refs()
            .update_ref(v1::UpdateRefRequest::default())
            .await
            .map(|_| ()),
    );
    // Served-but-unimplemented methods are recorded; unknown services are not.
    let methods: Vec<String> = up.requests().into_iter().map(|r| r.method).collect();
    assert!(methods.contains(&"/scion.v1.QueryService/ListTree".to_owned()));
    assert!(methods.contains(&"/scion.v1.AdminService/Redact".to_owned()));
    assert!(!methods.iter().any(|m| m.contains("WriteService")));
    // UNIMPLEMENTED responses still carry the epoch trailers.
    assert!(c.last_epochs().is_some());
}
