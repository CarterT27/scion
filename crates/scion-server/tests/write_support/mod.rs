//! Shared helpers of the P26 write and ref RPC tests.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use scion_core::{ErrorCode, ErrorPayload, ObjectId, Principal, ScionError};
use scion_object::RefName;
use scion_proto::convert::{DELTA_OP_DELETE, DELTA_OP_PUT};
use scion_proto::{ScionClient, v1};
use scion_server::testing::{DEFAULT_ACL, InProcessServer, ServerTestOpts};
use std::sync::Arc;

/// Test options: default ACL and registry, rate limits raised so concurrency tests are never
/// shed by the per-principal governor.
pub fn opts() -> ServerTestOpts {
    opts_with_acl(DEFAULT_ACL)
}

/// [`opts`] with a custom `acl.toml`.
pub fn opts_with_acl(acl: &str) -> ServerTestOpts {
    ServerTestOpts {
        acl_toml: acl.to_owned(),
        tweak: Some(Arc::new(|c| {
            c.rate_limit.per_principal_rps = 1_000_000;
            c.rate_limit.burst = 1_000_000;
            c.rate_limit.max_inflight = 1_000_000;
        })),
        ..ServerTestOpts::default()
    }
}

/// An ACL with a restricted `/secrets` prefix (only `github:admin` reads/writes it), ref
/// patterns with distinct `update` / `force` grants, and `admin.refs` for admins only.
/// `github:reader` may read but not write; `github:mallory` has no grant at all.
pub const RESTRICTED_ACL: &str = r#"version = 1

[groups]
eng = ["github:alice", "github:bob", "service:ci", "group:admins"]
admins = ["github:admin"]
readers = ["github:reader", "group:eng"]

[[paths]]
prefix = "/"
read = ["group:readers"]
write = ["group:eng"]

[[paths]]
prefix = "/secrets"
read = ["github:admin"]
write = ["github:admin"]

[refs."main"]
update = ["group:eng"]
force = []

[refs."release/*"]
update = ["group:admins"]
force = ["group:admins"]

[refs."green/*"]
update = ["service:ci"]
force = ["service:ci"]

[refs."*"]
update = ["group:eng"]
force = ["group:admins"]

[admin]
redact = ["group:admins"]
purge = ["group:admins"]
refs = ["group:admins"]
"#;

pub fn gh(login: &str) -> Principal {
    Principal::GitHub {
        login: login.into(),
        id: None,
    }
}

pub fn alice() -> Principal {
    gh("alice")
}

pub fn bob() -> Principal {
    gh("bob")
}

pub fn admin() -> Principal {
    gh("admin")
}

/// A principal with a valid token and no grant in any test ACL.
pub fn mallory() -> Principal {
    gh("mallory")
}

pub fn ci() -> Principal {
    Principal::Service("ci".into())
}

/// Change id bytes from a small integer.
pub fn cid(n: u32) -> Vec<u8> {
    let mut b = vec![0xC0; 16];
    b[12..].copy_from_slice(&n.to_be_bytes());
    b
}

/// Meta fields with a fixed author and time.
pub fn meta(msg: &str) -> v1::MetaFields {
    v1::MetaFields {
        author: b"Test Author <test@example.com>".to_vec(),
        author_time: Some(v1::Timestamp {
            seconds: 1_700_000_000,
            tz_offset_minutes: 0,
        }),
        committer: None,
        committer_time: None,
        message: msg.as_bytes().to_vec(),
        extensions: Vec::new(),
    }
}

/// Store `content` as a blob directly through the server's object facade (UploadObjects is
/// another package's RPC). Returns the id and size.
pub async fn blob(srv: &InProcessServer, content: &[u8]) -> (ObjectId, u64) {
    let (id, _) = srv
        .ctx()
        .objects
        .put_blob(Bytes::copy_from_slice(content))
        .await
        .unwrap();
    (id, content.len() as u64)
}

pub fn put_entry(path: &str, id: &ObjectId, size: u64) -> v1::DeltaEntryMsg {
    v1::DeltaEntryMsg {
        path: path.as_bytes().to_vec(),
        op: DELTA_OP_PUT,
        mode: 1,
        id: id.as_bytes().to_vec(),
        size,
    }
}

pub fn del_entry(path: &str) -> v1::DeltaEntryMsg {
    v1::DeltaEntryMsg {
        path: path.as_bytes().to_vec(),
        op: DELTA_OP_DELETE,
        mode: 0,
        id: Vec::new(),
        size: 0,
    }
}

/// A CreateChangeset request.
pub fn create_req(
    parents: &[ObjectId],
    delta: Vec<v1::DeltaEntryMsg>,
    change_id: u32,
) -> v1::CreateChangesetRequest {
    v1::CreateChangesetRequest {
        parents: parents.iter().map(|p| p.as_bytes().to_vec()).collect(),
        delta,
        meta_fields: Some(meta(&format!("change {change_id}"))),
        change_id: cid(change_id),
        expected_root_tree: None,
        import_mode: false,
    }
}

/// Create a changeset putting `files` (path, content) on `parent`, as the client's principal.
pub async fn commit(
    srv: &InProcessServer,
    c: &ScionClient,
    parent: &ObjectId,
    files: &[(&str, &[u8])],
    change_id: u32,
) -> ObjectId {
    let mut delta = Vec::new();
    for (path, content) in files {
        let (id, size) = blob(srv, content).await;
        delta.push(put_entry(path, &id, size));
    }
    delta.sort_by(|a, b| a.path.cmp(&b.path));
    let r = c
        .write()
        .create_changeset(create_req(&[*parent], delta, change_id))
        .await
        .unwrap()
        .into_inner();
    ObjectId::from_bytes(&r.changeset_id).unwrap()
}

/// The current target and version of `name`.
pub async fn ref_state(srv: &InProcessServer, name: &str) -> (ObjectId, u64) {
    let r = srv
        .ctx()
        .history
        .resolve_ref(&RefName::parse(name).unwrap(), None)
        .await
        .unwrap();
    (r.target, r.version)
}

/// The root changeset (main@1 after bootstrap).
pub async fn root(srv: &InProcessServer) -> ObjectId {
    ref_state(srv, "main").await.0
}

pub fn id(b: &[u8]) -> ObjectId {
    ObjectId::from_bytes(b).unwrap()
}

/// The ScionError carried by a status (SPEC 7.2 ErrorDetail).
pub fn err(s: &tonic::Status) -> ScionError {
    scion_proto::error_from_status(s)
}

/// Assert the status carries `code` (and `subcode` when given); returns the decoded error.
pub fn expect_err(s: &tonic::Status, code: ErrorCode, sub: Option<&str>) -> ScionError {
    let e = err(s);
    assert_eq!(e.code, code, "unexpected error: {e:?}");
    assert_eq!(
        s.code(),
        scion_proto::tonic_code(code.grpc_code()),
        "grpc code"
    );
    if let Some(sub) = sub {
        assert_eq!(e.subcode(), Some(sub), "unexpected subcode: {e:?}");
    }
    e
}

/// The `RefMismatch` payload of a REF_VERSION_MISMATCH error.
pub fn ref_mismatch(e: &ScionError) -> (u64, Option<ObjectId>) {
    match &e.payload {
        ErrorPayload::RefMismatch {
            current_version,
            current_target,
        } => (*current_version, *current_target),
        other => panic!("expected RefMismatch payload, got {other:?}"),
    }
}

/// Start a server with [`opts`].
pub async fn start() -> InProcessServer {
    InProcessServer::start(opts()).await
}
