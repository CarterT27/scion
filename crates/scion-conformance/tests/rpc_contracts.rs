//! SPEC 19.7 RPC contracts and error model (P30), on the Memory backend and, as `external_*`,
//! on CockroachDB + S3 when `SCION_TEST_CRDB_URL` and `SCION_TEST_S3_ENDPOINT` are set.
//!
//! The matrix is descriptor-driven: every RPC of `scion_proto::v1::FILE_DESCRIPTOR_SET` is
//! dispatched to its case by method name, and an RPC without a case fails
//! [`every_descriptor_rpc_has_a_contract_case`]. Each case runs through `Via::Server` and
//! `Via::Daemon` and covers the success path, every error code SPEC 7.2-7.7 (and the handler
//! contracts) list for it, and a request with unknown fields (ignored, SPEC 19.7). Generic
//! sections cover authentication failure (missing, malformed, expired, tampered and
//! wrong-key tokens) and missing required fields (INVALID_ARGUMENT/missing_field) for every
//! RPC, RESOURCE_EXHAUSTED (rate limiting) and UNAVAILABLE (injected storage faults).
//!
//! Every observed error is checked against the SPEC 7.2 table (gRPC status, `ErrorDetail.code`
//! and `ErrorDetail.retryable`), and every response, success or error, from either tier must
//! carry the three trailers with the right `scion-tier` (PLAN 6.13: forwarded RPCs keep
//! `server`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use prost::Message;
use scion_conformance::{Backend, ClusterOpts, TestCluster, Via, invariants};
use scion_core::{ErrorCode, ErrorPayload, FaultAction, Kind, ObjectId, Principal, Tier, subcode};
use scion_proto::v1;
use scion_store::Area;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tonic::metadata::MetadataMap;

// ---------------------------------------------------------------------------------------
// The SPEC 7.2 table (independent of `ErrorCode::grpc_code` / `retryable`).
// ---------------------------------------------------------------------------------------

const SPEC_7_2: &[(&str, tonic::Code, bool)] = &[
    ("NOT_FOUND", tonic::Code::NotFound, false),
    ("PERMISSION_DENIED", tonic::Code::PermissionDenied, false),
    ("UNAUTHENTICATED", tonic::Code::Unauthenticated, false),
    ("INVALID_ARGUMENT", tonic::Code::InvalidArgument, false),
    ("TOO_LARGE", tonic::Code::InvalidArgument, false),
    ("INTEGRITY", tonic::Code::DataLoss, true),
    ("REDACTED", tonic::Code::FailedPrecondition, false),
    ("MERGE_CONFLICT", tonic::Code::Aborted, false),
    ("AMBIGUOUS_MERGE_BASE", tonic::Code::FailedPrecondition, false),
    ("REF_VERSION_MISMATCH", tonic::Code::Aborted, true),
    ("NOT_FAST_FORWARD", tonic::Code::FailedPrecondition, false),
    ("ALREADY_LANDED", tonic::Code::AlreadyExists, false),
    ("EMPTY_CHANGE", tonic::Code::FailedPrecondition, false),
    ("RESOURCE_EXHAUSTED", tonic::Code::ResourceExhausted, true),
    ("UNAVAILABLE", tonic::Code::Unavailable, true),
];

fn spec_row(code: &str) -> (tonic::Code, bool) {
    SPEC_7_2
        .iter()
        .find(|(c, _, _)| *c == code)
        .map(|(_, g, r)| (*g, *r))
        .unwrap_or_else(|| panic!("{code} is not a SPEC 7.2 code"))
}

#[test]
fn spec_table_matches_error_code() {
    assert_eq!(SPEC_7_2.len(), ErrorCode::ALL.len());
    for c in ErrorCode::ALL {
        let (g, r) = spec_row(c.as_str());
        assert_eq!(scion_proto::tonic_code(c.grpc_code()), g, "{c:?}");
        assert_eq!(c.retryable(), r, "{c:?}");
    }
}

/// The methods the daemon answers itself (PLAN 6.13); everything else is forwarded.
fn tier_of(via: Via, method: &str) -> Tier {
    match (via, method) {
        (Via::Daemon, "GetServerInfo" | "GetObjects" | "ResolveRef") => Tier::Daemon,
        _ => Tier::Server,
    }
}

fn check_trailers(md: &MetadataMap, tier: Tier, what: &str) {
    let e = TestCluster::epochs(md).unwrap_or_else(|| panic!("{what}: missing trailers"));
    assert_eq!(e.tier, tier, "{what}: scion-tier");
}

/// Assert that `s` is the SPEC 7.2 error `code` (with `sub` when given), retryable exactly
/// as the table says, with trailers of `tier`. Returns the decoded error.
fn check_err(
    s: &tonic::Status,
    code: ErrorCode,
    sub: Option<&str>,
    tier: Tier,
    what: &str,
) -> scion_core::ScionError {
    let (grpc, retryable) = spec_row(code.as_str());
    assert_eq!(s.code(), grpc, "{what}: gRPC status for {code:?} ({s:?})");
    let d = TestCluster::error_detail(s).unwrap_or_else(|| panic!("{what}: no ErrorDetail"));
    assert_eq!(d.code, code.as_str(), "{what}: ErrorDetail.code ({s:?})");
    assert_eq!(d.retryable, retryable, "{what}: ErrorDetail.retryable");
    check_trailers(s.metadata(), tier, what);
    let e = scion_proto::error_from_status(s);
    if let Some(sub) = sub {
        assert_eq!(e.subcode(), Some(sub), "{what}: subcode ({e:?})");
    }
    e
}

/// The success value of a unary call, after checking its trailers.
fn ok<T>(r: Result<tonic::Response<T>, tonic::Status>, tier: Tier, what: &str) -> T {
    match r {
        Ok(resp) => {
            check_trailers(resp.metadata(), tier, what);
            resp.into_inner()
        }
        Err(s) => panic!("{what}: unexpected error {s:?}"),
    }
}

fn err<T: std::fmt::Debug>(
    r: Result<tonic::Response<T>, tonic::Status>,
    code: ErrorCode,
    sub: Option<&str>,
    tier: Tier,
    what: &str,
) -> scion_core::ScionError {
    match r {
        Ok(v) => panic!("{what}: expected {code:?}, got {:?}", v.into_inner()),
        Err(s) => check_err(&s, code, sub, tier, what),
    }
}

/// Drain a server-streaming response: the messages plus the trailers, or the final status.
async fn drain<T>(
    r: Result<tonic::Response<tonic::Streaming<T>>, tonic::Status>,
) -> Result<(Vec<T>, MetadataMap), tonic::Status> {
    let resp = r?;
    let mut md = resp.metadata().clone();
    let mut s = resp.into_inner();
    let mut out = Vec::new();
    while let Some(m) = s.message().await? {
        out.push(m);
    }
    if let Some(t) = s.trailers().await? {
        let mut h = md.into_headers();
        h.extend(t.into_headers());
        md = MetadataMap::from_headers(h);
    }
    Ok((out, md))
}

fn ok_stream<T>(r: Result<(Vec<T>, MetadataMap), tonic::Status>, tier: Tier, what: &str) -> Vec<T> {
    match r {
        Ok((v, md)) => {
            check_trailers(&md, tier, what);
            v
        }
        Err(s) => panic!("{what}: unexpected error {s:?}"),
    }
}

fn err_stream<T>(
    r: Result<(Vec<T>, MetadataMap), tonic::Status>,
    code: ErrorCode,
    sub: Option<&str>,
    tier: Tier,
    what: &str,
) {
    match r {
        Ok((v, _)) => panic!("{what}: expected {code:?}, got {} messages", v.len()),
        Err(s) => {
            check_err(&s, code, sub, tier, what);
        }
    }
}

/// Append unknown fields (a varint and a length-delimited field with numbers no scion.v1
/// message uses) to an encoded message (SPEC 19.7 forward compatibility).
fn with_unknown_fields(mut body: Vec<u8>) -> Bytes {
    prost::encoding::encode_key(1999, prost::encoding::WireType::Varint, &mut body);
    prost::encoding::encode_varint(42, &mut body);
    prost::encoding::encode_key(2000, prost::encoding::WireType::LengthDelimited, &mut body);
    prost::encoding::encode_varint(3, &mut body);
    body.extend_from_slice(b"new");
    Bytes::from(body)
}

fn gh(login: &str) -> Principal {
    TestCluster::gh(login)
}

fn idb(id: &ObjectId) -> Vec<u8> {
    id.as_bytes().to_vec()
}

fn skip_external() -> bool {
    if Backend::external_available() {
        return false;
    }
    println!("SKIP: SCION_TEST_CRDB_URL / SCION_TEST_S3_ENDPOINT unset");
    true
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------------------
// Environment.
// ---------------------------------------------------------------------------------------

/// Largest blob the matrix servers accept (so TOO_LARGE is cheap to provoke).
const MAX_BLOB: u64 = 4 * 1024 * 1024;

struct Env {
    c: TestCluster,
    /// `main` after the fixture landed: README.md, docs/a.txt, secrets/s.txt, notes/red.txt.
    c1: ObjectId,
    /// The redacted blob (notes/red.txt).
    red: ObjectId,
}

impl Env {
    async fn start(backend: Backend) -> Env {
        let mut o = ClusterOpts::new(backend);
        let base = o.server.tweak.clone();
        o.server.tweak = Some(Arc::new(move |cfg| {
            if let Some(t) = &base {
                t(cfg);
            }
            cfg.limits.max_blob_bytes = MAX_BLOB;
        }));
        o.daemon_tweak = Some(Arc::new(|d| d.redaction_poll_interval_ms = 100));
        let c = TestCluster::start(o).await;
        let admin = c.client(Via::Server, &gh("admin")).await;
        let (root, _) = c.ref_head("main").await;
        let c1 = TestCluster::commit(
            &admin,
            &root,
            &[
                ("README.md", b"readme\n"),
                ("docs/a.txt", b"a1\na2\na3\n"),
                ("notes/red.txt", b"red\n"),
                ("secrets/s.txt", b"secret\n"),
            ],
            &[],
            1,
        )
        .await;
        ok(
            admin
                .write()
                .land(v1::LandRequest {
                    r#ref: "main".into(),
                    head: idb(&c1),
                    allow_empty: false,
                })
                .await,
            Tier::Server,
            "fixture land",
        );
        let red = ObjectId::hash_blob(b"red\n");
        ok(
            admin
                .admin()
                .redact(v1::RedactRequest {
                    object_ids: vec![idb(&red)],
                    reason: "fixture".into(),
                    purge_after_seconds: None,
                })
                .await,
            Tier::Server,
            "fixture redact",
        );
        let env = Env { c, c1, red };
        env.sync_daemon().await;
        env
    }

    /// Wait until the daemon's redaction set reached the server's epoch.
    async fn sync_daemon(&self) {
        let alice = self.client(Via::Daemon, "alice").await;
        let _ = alice
            .objects()
            .resolve_ref(v1::ResolveRefRequest {
                name: "main".into(),
                at_version: None,
            })
            .await;
        let want = self.c.servers[0].ctx().redactions.epoch();
        let d = self.c.daemon.as_ref().unwrap();
        eventually("daemon redaction sync", || {
            d.handle.stats().redaction_epoch >= want
        })
        .await;
    }

    async fn client(&self, via: Via, who: &str) -> scion_proto::ScionClient {
        self.c.client(via, &gh(who)).await
    }

    async fn main(&self) -> ObjectId {
        self.c.ref_head("main").await.0
    }

    /// Raw call of `rpc` with `body` plus unknown fields: must succeed (SPEC 19.7).
    async fn unknown_fields_ok(&self, via: Via, method: &str, body: Vec<u8>, who: &str) {
        let rpc = TestCluster::rpcs()
            .into_iter()
            .find(|r| r.method == method)
            .unwrap();
        let client = self.c.client_with_token(via, None).await;
        let token = (who != "-").then(|| self.c.token_for(&gh(who)));
        let (status, md) = TestCluster::raw_call(
            client.channel(),
            &rpc,
            Some(with_unknown_fields(body)),
            token.as_deref(),
        )
        .await;
        assert_eq!(
            status.code(),
            tonic::Code::Ok,
            "{method} via {}: unknown fields must be ignored ({status:?})",
            via.as_str()
        );
        check_trailers(&md, tier_of(via, method), &format!("{method} unknown fields"));
    }
}

/// Unique bytes for case `what` on `via` with salt `s` (cases run once per tier).
fn uniq(what: &str, via: Via, s: u32) -> Vec<u8> {
    format!("{what} {} {s}\n", via.as_str()).into_bytes()
}

/// A change id unique per case and tier.
fn cid(via: Via, n: u32) -> u32 {
    match via {
        Via::Server => 10_000 + n,
        Via::Daemon => 20_000 + n,
    }
}

async fn land(env: &Env, via: Via, who: &str, head: &ObjectId) -> Result<v1::LandResponse, tonic::Status> {
    env.client(via, who)
        .await
        .write()
        .land(v1::LandRequest {
            r#ref: "main".into(),
            head: idb(head),
            allow_empty: false,
        })
        .await
        .map(|r| r.into_inner())
}

// ---------------------------------------------------------------------------------------
// Cases: ObjectService.
// ---------------------------------------------------------------------------------------

async fn case_get_server_info(env: &Env, via: Via) {
    let t = tier_of(via, "GetServerInfo");
    let anon = env.c.client_with_token(via, None).await;
    let info = ok(
        anon.objects().get_server_info(v1::GetServerInfoRequest {}).await,
        t,
        "GetServerInfo without a token",
    );
    assert_eq!(info.protocol_versions, vec!["scion.v1".to_owned()]);
    assert!(!info.token_public_keys.is_empty());
    assert_eq!(info.limits.unwrap().max_blob_bytes, MAX_BLOB);
    env.unknown_fields_ok(via, "GetServerInfo", Vec::new(), "-").await;
}

async fn case_get_objects(env: &Env, via: Via) {
    let t = tier_of(via, "GetObjects");
    let alice = env.client(via, "alice").await;
    let readme = ObjectId::hash_blob(b"readme\n");
    let secret = ObjectId::hash_blob(b"secret\n");
    let absent = ObjectId::hash_blob(b"absent object\n");
    // SPEC 19.7: one batch mixing ok, not_found, denied and redacted.
    let f = TestCluster::fetch(&alice, &[readme, absent, secret, env.red], None, None)
        .await
        .unwrap();
    check_trailers(&f.metadata, t, "GetObjects mixed batch");
    let st: Vec<i32> = f.items.iter().map(|i| i.status).collect();
    assert_eq!(
        st,
        vec![
            v1::ItemStatus::Ok as i32,
            v1::ItemStatus::NotFound as i32,
            v1::ItemStatus::Denied as i32,
            v1::ItemStatus::Redacted as i32
        ]
    );
    assert!(f.items[0].verified && f.items[0].content == b"readme\n");
    // Range read: raw bytes.
    let f = TestCluster::fetch(&alice, &[readme], Some(1), Some(3)).await.unwrap();
    assert_eq!(f.items[0].content, b"ead");
    assert_eq!(f.items[0].encoding, v1::Encoding::Raw as i32);
    assert_eq!(f.items[0].total_size, 7);
    // INVALID_ARGUMENT: malformed id.
    err_stream(
        drain(
            alice
                .objects()
                .get_objects(v1::GetObjectsRequest {
                    ids: vec![vec![1, 2, 3]],
                    offset: None,
                    length: None,
                })
                .await,
        )
        .await,
        ErrorCode::InvalidArgument,
        None,
        t,
        "GetObjects malformed id",
    );
    // INTEGRITY: the server's own copy is corrupt (SPEC 16.1 class 7); other items are still
    // served and the stream ends with INTEGRITY.
    let content = uniq("corrupt", via, 1);
    let admin = env.client(Via::Server, "admin").await;
    let main = env.main().await;
    let path = format!("docs/corrupt-{}.txt", via.as_str());
    TestCluster::commit(&admin, &main, &[(&path, &content)], &[], cid(via, 1)).await;
    let bad = ObjectId::hash_blob(&content);
    let ctx = env.c.servers[0].ctx();
    let key = ctx.objects.key(&bad, Area::Live);
    let frame = zstd::encode_all(&b"tampered bytes\n"[..], 3).unwrap();
    env.c.storage.blobs.put(&key, Bytes::from(frame)).await.unwrap();
    let r = alice
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: vec![idb(&readme), idb(&bad)],
            offset: None,
            length: None,
        })
        .await;
    match drain(r).await {
        Ok((items, _)) => panic!("GetObjects corrupt: expected INTEGRITY, got {items:?}"),
        Err(s) => {
            check_err(&s, ErrorCode::Integrity, None, t, "GetObjects corrupt copy");
        }
    }
    // Repair the stored copy so the run ends with the 16.2 invariants intact.
    let good = zstd::encode_all(&content[..], 3).unwrap();
    env.c.storage.blobs.put(&key, Bytes::from(good)).await.unwrap();
    let body = v1::GetObjectsRequest {
        ids: vec![idb(&readme)],
        offset: None,
        length: None,
    }
    .encode_to_vec();
    env.unknown_fields_ok(via, "GetObjects", body, "alice").await;
}

async fn case_has_objects(env: &Env, via: Via) {
    let t = tier_of(via, "HasObjects");
    let alice = env.client(via, "alice").await;
    let r = ok(
        alice
            .objects()
            .has_objects(v1::HasObjectsRequest {
                ids: vec![
                    idb(&ObjectId::hash_blob(b"readme\n")),
                    idb(&ObjectId::hash_blob(b"secret\n")),
                    idb(&ObjectId::hash_blob(b"absent\n")),
                    idb(&env.red),
                ],
            })
            .await,
        t,
        "HasObjects",
    );
    assert_eq!(r.present, vec![true, false, false, false]);
    err(
        alice
            .objects()
            .has_objects(v1::HasObjectsRequest {
                ids: vec![b"short".to_vec()],
            })
            .await,
        ErrorCode::InvalidArgument,
        None,
        t,
        "HasObjects malformed id",
    );
    let body = v1::HasObjectsRequest {
        ids: vec![idb(&env.red)],
    }
    .encode_to_vec();
    env.unknown_fields_ok(via, "HasObjects", body, "alice").await;
}

async fn case_resolve_ref(env: &Env, via: Via) {
    let t = tier_of(via, "ResolveRef");
    let alice = env.client(via, "alice").await;
    let r = ok(
        alice
            .objects()
            .resolve_ref(v1::ResolveRefRequest {
                name: "main".into(),
                at_version: Some(2),
            })
            .await,
        t,
        "ResolveRef",
    );
    assert_eq!((ObjectId::from_bytes(&r.target).unwrap(), r.version), (env.c1, 2));
    err(
        alice
            .objects()
            .resolve_ref(v1::ResolveRefRequest {
                name: "no-such-ref".into(),
                at_version: None,
            })
            .await,
        ErrorCode::NotFound,
        None,
        t,
        "ResolveRef absent",
    );
    err(
        alice
            .objects()
            .resolve_ref(v1::ResolveRefRequest {
                name: "bad..name".into(),
                at_version: None,
            })
            .await,
        ErrorCode::InvalidArgument,
        Some(subcode::INVALID_REF_NAME),
        t,
        "ResolveRef malformed name",
    );
    let body = v1::ResolveRefRequest {
        name: "main".into(),
        at_version: None,
    }
    .encode_to_vec();
    env.unknown_fields_ok(via, "ResolveRef", body, "alice").await;
}

// ---------------------------------------------------------------------------------------
// Cases: QueryService.
// ---------------------------------------------------------------------------------------

async fn case_get_changesets(env: &Env, via: Via) {
    let t = tier_of(via, "GetChangesets");
    let alice = env.client(via, "alice").await;
    let carol = env.client(via, "carol").await;
    let req = v1::GetChangesetsRequest {
        ids: vec![idb(&env.c1), idb(&ObjectId::hash_blob(b"no changeset"))],
    };
    // alice cannot read secrets/s.txt: filtered view; carol gets the canonical encoding.
    let r = ok(alice.query().get_changesets(req.clone()).await, t, "GetChangesets");
    assert_eq!(r.items[0].status, v1::ItemStatus::Ok as i32);
    match &r.items[0].view {
        Some(v1::changeset_item::View::FilteredView(f)) => {
            assert!(f.filtered);
            assert!(f.delta.iter().all(|d| !d.path.starts_with(b"secrets")));
        }
        other => panic!("expected a filtered view, got {other:?}"),
    }
    assert_eq!(r.items[1].status, v1::ItemStatus::NotFound as i32);
    let r = ok(carol.query().get_changesets(req.clone()).await, t, "GetChangesets full");
    assert!(matches!(
        r.items[0].view,
        Some(v1::changeset_item::View::Encoded(_))
    ));
    err(
        alice
            .query()
            .get_changesets(v1::GetChangesetsRequest {
                ids: vec![vec![0xFF; 3]],
            })
            .await,
        ErrorCode::InvalidArgument,
        Some(subcode::BAD_ID_TEXT),
        t,
        "GetChangesets malformed id",
    );
    let mallory = env.client(via, "mallory").await;
    err(
        mallory.query().get_changesets(req.clone()).await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "GetChangesets without repository read",
    );
    env.unknown_fields_ok(via, "GetChangesets", req.encode_to_vec(), "alice")
        .await;
}

fn read_path_req(cs: &ObjectId, path: &str) -> v1::ReadPathRequest {
    v1::ReadPathRequest {
        changeset: idb(cs),
        path: path.as_bytes().to_vec(),
    }
}

async fn case_read_path(env: &Env, via: Via) {
    let t = tier_of(via, "ReadPath");
    let alice = env.client(via, "alice").await;
    let chunks = ok_stream(
        drain(alice.query().read_path(read_path_req(&env.c1, "docs/a.txt")).await).await,
        t,
        "ReadPath",
    );
    let data: Vec<u8> = chunks.iter().flat_map(|c| c.data.clone()).collect();
    assert_eq!(data, b"a1\na2\na3\n");
    assert!(chunks[0].entry.is_some() && chunks.last().unwrap().r#final);
    for (what, req, code, sub) in [
        (
            "ReadPath missing path",
            read_path_req(&env.c1, "docs/none.txt"),
            ErrorCode::NotFound,
            None,
        ),
        (
            "ReadPath missing changeset",
            read_path_req(&ObjectId::hash_encoded(Kind::Changeset, b"x"), "README.md"),
            ErrorCode::NotFound,
            None,
        ),
        (
            "ReadPath restricted",
            read_path_req(&env.c1, "secrets/s.txt"),
            ErrorCode::PermissionDenied,
            None,
        ),
        (
            "ReadPath redacted",
            read_path_req(&env.c1, "notes/red.txt"),
            ErrorCode::Redacted,
            None,
        ),
        (
            "ReadPath malformed path",
            read_path_req(&env.c1, "docs//a.txt"),
            ErrorCode::InvalidArgument,
            Some(subcode::INVALID_PATH),
        ),
    ] {
        err_stream(
            drain(alice.query().read_path(req).await).await,
            code,
            sub,
            t,
            what,
        );
    }
    env.unknown_fields_ok(
        via,
        "ReadPath",
        read_path_req(&env.c1, "README.md").encode_to_vec(),
        "alice",
    )
    .await;
}

fn list_req(cs: &ObjectId, path: &str) -> v1::ListTreeRequest {
    v1::ListTreeRequest {
        changeset: idb(cs),
        path: path.as_bytes().to_vec(),
    }
}

async fn case_list_tree(env: &Env, via: Via) {
    let t = tier_of(via, "ListTree");
    let alice = env.client(via, "alice").await;
    let r = ok(alice.query().list_tree(list_req(&env.c1, "")).await, t, "ListTree");
    let names: Vec<&[u8]> = r.entries.iter().map(|e| e.name.as_slice()).collect();
    // SPEC 12.6: restricted subdirectories are listed (name and id) in readable parents.
    assert_eq!(names, vec![&b"README.md"[..], b"docs", b"notes", b"secrets"]);
    for (what, req, code, sub) in [
        (
            "ListTree missing path",
            list_req(&env.c1, "nope"),
            ErrorCode::NotFound,
            None,
        ),
        (
            "ListTree restricted",
            list_req(&env.c1, "secrets"),
            ErrorCode::PermissionDenied,
            None,
        ),
        (
            "ListTree of a file",
            list_req(&env.c1, "README.md"),
            ErrorCode::InvalidArgument,
            Some(subcode::INVALID_PATH),
        ),
        (
            "ListTree missing changeset",
            list_req(&ObjectId::hash_encoded(Kind::Changeset, b"y"), ""),
            ErrorCode::NotFound,
            None,
        ),
    ] {
        err(alice.query().list_tree(req).await, code, sub, t, what);
    }
    env.unknown_fields_ok(via, "ListTree", list_req(&env.c1, "docs").encode_to_vec(), "alice")
        .await;
}

fn log_req(from: &ObjectId, path: Option<&str>) -> v1::LogRequest {
    v1::LogRequest {
        from: idb(from),
        path: path.map(|p| p.as_bytes().to_vec()),
        first_parent_only: false,
        limit: 1,
        page_token: Vec::new(),
    }
}

async fn case_log(env: &Env, via: Via) {
    let t = tier_of(via, "Log");
    let alice = env.client(via, "alice").await;
    let r = ok(alice.query().log(log_req(&env.c1, None)).await, t, "Log");
    assert_eq!(r.entries.len(), 1);
    assert!(!r.next_page_token.is_empty());
    let mut next = log_req(&env.c1, None);
    next.page_token = r.next_page_token.clone();
    let r2 = ok(alice.query().log(next).await, t, "Log page 2");
    assert_eq!(r2.entries.len(), 1);
    let r = ok(
        alice.query().log(log_req(&env.c1, Some("docs/a.txt"))).await,
        t,
        "Log(path)",
    );
    assert_eq!(r.entries.len(), 1);
    let mut bad_token = log_req(&env.c1, None);
    bad_token.page_token = b"garbage".to_vec();
    err(
        alice.query().log(bad_token).await,
        ErrorCode::InvalidArgument,
        Some(subcode::BAD_PAGE_TOKEN),
        t,
        "Log bad page token",
    );
    err(
        alice
            .query()
            .log(log_req(&ObjectId::hash_encoded(Kind::Changeset, b"z"), None))
            .await,
        ErrorCode::NotFound,
        None,
        t,
        "Log missing from",
    );
    err(
        alice
            .query()
            .log(log_req(&env.c1, Some("secrets/s.txt")))
            .await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "Log(restricted path)",
    );
    let mallory = env.client(via, "mallory").await;
    err(
        mallory.query().log(log_req(&env.c1, None)).await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "Log without repository read",
    );
    env.unknown_fields_ok(via, "Log", log_req(&env.c1, None).encode_to_vec(), "alice")
        .await;
}

fn blame_req(cs: &ObjectId, path: &str) -> v1::BlameRequest {
    v1::BlameRequest {
        changeset: idb(cs),
        path: path.as_bytes().to_vec(),
    }
}

async fn case_blame(env: &Env, via: Via) {
    let t = tier_of(via, "Blame");
    let alice = env.client(via, "alice").await;
    let r = ok(
        alice.query().blame(blame_req(&env.c1, "docs/a.txt")).await,
        t,
        "Blame",
    );
    assert_eq!(r.ranges.len(), 1);
    assert_eq!((r.ranges[0].start_line, r.ranges[0].line_count), (0, 3));
    assert_eq!(r.ranges[0].origin_changeset, idb(&env.c1));
    for (what, req, code, sub) in [
        (
            "Blame missing path",
            blame_req(&env.c1, "docs/none.txt"),
            ErrorCode::NotFound,
            None,
        ),
        (
            "Blame directory",
            blame_req(&env.c1, "docs"),
            ErrorCode::NotFound,
            None,
        ),
        (
            "Blame restricted",
            blame_req(&env.c1, "secrets/s.txt"),
            ErrorCode::PermissionDenied,
            None,
        ),
        (
            "Blame malformed path",
            blame_req(&env.c1, "/abs"),
            ErrorCode::InvalidArgument,
            Some(subcode::INVALID_PATH),
        ),
        (
            "Blame malformed changeset",
            v1::BlameRequest {
                changeset: vec![9; 5],
                path: b"README.md".to_vec(),
            },
            ErrorCode::InvalidArgument,
            Some(subcode::BAD_ID_TEXT),
        ),
    ] {
        err(alice.query().blame(req).await, code, sub, t, what);
    }
    env.unknown_fields_ok(
        via,
        "Blame",
        blame_req(&env.c1, "README.md").encode_to_vec(),
        "alice",
    )
    .await;
}

fn diff_req(a: Option<&ObjectId>, b: &ObjectId, paths: &[&str]) -> v1::DiffRequest {
    v1::DiffRequest {
        a: a.map(idb).unwrap_or_default(),
        b: idb(b),
        paths: paths.iter().map(|p| p.as_bytes().to_vec()).collect(),
        include_content_hunks: true,
    }
}

async fn case_diff(env: &Env, via: Via) {
    let t = tier_of(via, "Diff");
    let alice = env.client(via, "alice").await;
    let (root, _) = (env.c.servers[0].ctx().history.resolve_ref(
        &scion_object::RefName::parse("main").unwrap(),
        Some(1),
    ))
    .await
    .map(|r| (r.target, r.version))
    .unwrap();
    let items = ok_stream(
        drain(alice.query().diff(diff_req(Some(&root), &env.c1, &[])).await).await,
        t,
        "Diff",
    );
    let paths: Vec<&[u8]> = items.iter().map(|i| i.path.as_slice()).collect();
    // SPEC 12.6: unreadable entries are omitted.
    assert_eq!(paths, vec![&b"README.md"[..], b"docs/a.txt", b"notes/red.txt"]);
    for (what, req, code) in [
        (
            "Diff missing changeset",
            diff_req(None, &ObjectId::hash_encoded(Kind::Changeset, b"q"), &[]),
            ErrorCode::NotFound,
        ),
        (
            "Diff restricted scope",
            diff_req(Some(&root), &env.c1, &["secrets"]),
            ErrorCode::PermissionDenied,
        ),
        (
            "Diff missing b",
            v1::DiffRequest {
                a: idb(&root),
                b: Vec::new(),
                paths: Vec::new(),
                include_content_hunks: false,
            },
            ErrorCode::InvalidArgument,
        ),
    ] {
        err_stream(drain(alice.query().diff(req).await).await, code, None, t, what);
    }
    env.unknown_fields_ok(
        via,
        "Diff",
        diff_req(Some(&root), &env.c1, &[]).encode_to_vec(),
        "alice",
    )
    .await;
}

async fn case_is_ancestor(env: &Env, via: Via) {
    let t = tier_of(via, "IsAncestor");
    let alice = env.client(via, "alice").await;
    let main1 = env.c1;
    let req = |a: Vec<u8>, b: Vec<u8>| v1::IsAncestorRequest { a, b };
    let r = ok(
        alice.query().is_ancestor(req(idb(&main1), idb(&main1))).await,
        t,
        "IsAncestor",
    );
    assert!(r.result);
    err(
        alice
            .query()
            .is_ancestor(req(
                idb(&main1),
                idb(&ObjectId::hash_encoded(Kind::Changeset, b"m")),
            ))
            .await,
        ErrorCode::NotFound,
        None,
        t,
        "IsAncestor missing",
    );
    err(
        alice.query().is_ancestor(req(idb(&main1), vec![1])).await,
        ErrorCode::InvalidArgument,
        None,
        t,
        "IsAncestor malformed",
    );
    err(
        env.client(via, "mallory")
            .await
            .query()
            .is_ancestor(req(idb(&main1), idb(&main1)))
            .await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "IsAncestor without repository read",
    );
    env.unknown_fields_ok(
        via,
        "IsAncestor",
        req(idb(&main1), idb(&main1)).encode_to_vec(),
        "alice",
    )
    .await;
}

async fn case_merge_base(env: &Env, via: Via) {
    let t = tier_of(via, "MergeBase");
    let alice = env.client(via, "alice").await;
    let req = |a: Vec<u8>, b: Vec<u8>| v1::MergeBaseRequest { a, b };
    let r = ok(
        alice.query().merge_base(req(idb(&env.c1), idb(&env.c1))).await,
        t,
        "MergeBase",
    );
    assert_eq!(r.bases, vec![idb(&env.c1)]);
    err(
        alice
            .query()
            .merge_base(req(
                idb(&env.c1),
                idb(&ObjectId::hash_encoded(Kind::Changeset, b"n")),
            ))
            .await,
        ErrorCode::NotFound,
        None,
        t,
        "MergeBase missing",
    );
    err(
        alice.query().merge_base(req(Vec::new(), idb(&env.c1))).await,
        ErrorCode::InvalidArgument,
        Some(subcode::MISSING_FIELD),
        t,
        "MergeBase missing field",
    );
    err(
        env.client(via, "mallory")
            .await
            .query()
            .merge_base(req(idb(&env.c1), idb(&env.c1)))
            .await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "MergeBase without repository read",
    );
    env.unknown_fields_ok(
        via,
        "MergeBase",
        req(idb(&env.c1), idb(&env.c1)).encode_to_vec(),
        "alice",
    )
    .await;
}

async fn case_resolve(env: &Env, via: Via) {
    let t = tier_of(via, "Resolve");
    let alice = env.client(via, "alice").await;
    let req = |r: &str| v1::ResolveRequest {
        revision: r.to_owned(),
    };
    let r = ok(alice.query().resolve(req("main")).await, t, "Resolve");
    assert_eq!(r.changeset, idb(&env.main().await));
    let r = ok(alice.query().resolve(req(&env.c1.to_text())).await, t, "Resolve id");
    assert_eq!(r.changeset, idb(&env.c1));
    for (what, rev, code, sub) in [
        ("Resolve unknown", "no-such-thing", ErrorCode::NotFound, None),
        (
            "Resolve malformed",
            "main~~x!",
            ErrorCode::InvalidArgument,
            Some(subcode::INVALID_REVISION),
        ),
        (
            "Resolve empty",
            "",
            ErrorCode::InvalidArgument,
            Some(subcode::MISSING_FIELD),
        ),
    ] {
        err(alice.query().resolve(req(rev)).await, code, sub, t, what);
    }
    err(
        env.client(via, "mallory").await.query().resolve(req("main")).await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "Resolve without repository read",
    );
    env.unknown_fields_ok(via, "Resolve", req("main").encode_to_vec(), "alice")
        .await;
}

async fn case_get_derived(env: &Env, via: Via) {
    let t = tier_of(via, "GetDerived");
    let alice = env.client(via, "alice").await;
    let req = |ty: &str, key: Vec<u8>| v1::GetDerivedRequest {
        r#type: ty.to_owned(),
        version: None,
        key,
    };
    let r = ok(
        alice.query().get_derived(req("history", idb(&env.c1))).await,
        t,
        "GetDerived",
    );
    assert_eq!(r.version, 1);
    assert!(!r.value.is_empty());
    err(
        alice.query().get_derived(req("no-such-type", Vec::new())).await,
        ErrorCode::NotFound,
        None,
        t,
        "GetDerived unregistered type",
    );
    err(
        alice
            .query()
            .get_derived(req("history", idb(&ObjectId::hash_encoded(Kind::Changeset, b"d"))))
            .await,
        ErrorCode::NotFound,
        None,
        t,
        "GetDerived missing key",
    );
    err(
        alice.query().get_derived(req("", Vec::new())).await,
        ErrorCode::InvalidArgument,
        Some(subcode::MISSING_FIELD),
        t,
        "GetDerived missing type",
    );
    err(
        env.client(via, "mallory")
            .await
            .query()
            .get_derived(req("history", idb(&env.c1)))
            .await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "GetDerived without repository read",
    );
    env.unknown_fields_ok(
        via,
        "GetDerived",
        req("history", idb(&env.c1)).encode_to_vec(),
        "alice",
    )
    .await;
}

// ---------------------------------------------------------------------------------------
// Cases: WriteService.
// ---------------------------------------------------------------------------------------

fn upload_item(id: &ObjectId, kind: Kind, data: &[u8]) -> v1::UploadItem {
    v1::UploadItem {
        id: idb(id),
        kind: u32::from(kind.byte()),
        data: data.to_vec(),
    }
}

async fn case_upload_objects(env: &Env, via: Via) {
    let t = tier_of(via, "UploadObjects");
    let alice = env.client(via, "alice").await;
    let good = uniq("upload good", via, 1);
    let mismatch = uniq("upload mismatch", via, 2);
    let big = vec![7u8; MAX_BLOB as usize + 1];
    let items = vec![
        upload_item(&ObjectId::hash_blob(&good), Kind::Blob, &good),
        upload_item(&ObjectId::hash_blob(&mismatch), Kind::Blob, b"other bytes"),
        upload_item(&env.red, Kind::Blob, b"red\n"),
        upload_item(&ObjectId::hash_blob(&good), Kind::Tree, &good),
        upload_item(&ObjectId::hash_blob(&big), Kind::Blob, &big),
        // A redundant upload of an existing object is accepted (SPEC 12.5).
        upload_item(&ObjectId::hash_blob(b"secret\n"), Kind::Blob, b"secret\n"),
    ];
    let r = ok(
        alice.write().upload_objects(futures::stream::iter(items)).await,
        t,
        "UploadObjects",
    );
    let got: Vec<(i32, String)> = r
        .results
        .iter()
        .map(|x| (x.status, x.error_code.clone()))
        .collect();
    let okv = v1::ItemStatus::Ok as i32;
    assert_eq!(got[0], (okv, String::new()), "{got:?}");
    assert_eq!(got[1].1, "INTEGRITY", "{got:?}");
    assert_eq!(got[2].1, "REDACTED", "{got:?}");
    assert_eq!(got[3].1, "INVALID_ARGUMENT", "{got:?}");
    assert_eq!(got[4].1, "TOO_LARGE", "{got:?}");
    assert_eq!(got[5], (okv, String::new()), "{got:?}");
    assert!(got[1..5].iter().all(|(s, _)| *s != okv));
    let body = upload_item(&ObjectId::hash_blob(&good), Kind::Blob, &good).encode_to_vec();
    env.unknown_fields_ok(via, "UploadObjects", body, "alice").await;
}

async fn case_create_changeset(env: &Env, via: Via) {
    let t = tier_of(via, "CreateChangeset");
    let alice = env.client(via, "alice").await;
    let content = uniq("create", via, 1);
    let path = format!("w/{}/create.txt", via.as_str());
    TestCluster::upload(&alice, &[&content]).await;
    let req = TestCluster::create_request(&env.c1, &[(&path, &content)], &[], cid(via, 10));
    let r = ok(alice.write().create_changeset(req.clone()).await, t, "CreateChangeset");
    // Identical requests return the same identifiers (SPEC 8.1).
    let again = ok(alice.write().create_changeset(req.clone()).await, t, "CreateChangeset again");
    assert_eq!(r.changeset_id, again.changeset_id);

    let missing = uniq("never uploaded", via, 2);
    let mut long = TestCluster::create_request(&env.c1, &[(&path, &content)], &[], cid(via, 11));
    long.meta_fields.as_mut().unwrap().message = vec![b'x'; 1_048_577];
    let mut no_parent =
        TestCluster::create_request(&env.c1, &[(&path, &content)], &[], cid(via, 12));
    no_parent.parents = vec![idb(&ObjectId::hash_encoded(Kind::Changeset, b"p"))];
    let cases: Vec<(&str, &str, v1::CreateChangesetRequest, ErrorCode, Option<&str>)> = vec![
        (
            "missing blob",
            "alice",
            TestCluster::create_request(&env.c1, &[(&path, &missing)], &[], cid(via, 13)),
            ErrorCode::InvalidArgument,
            Some(subcode::MISSING_BLOB),
        ),
        (
            "missing parent",
            "alice",
            no_parent,
            ErrorCode::InvalidArgument,
            Some(subcode::MISSING_PARENT),
        ),
        (
            "message too long",
            "alice",
            long,
            ErrorCode::TooLarge,
            None,
        ),
        (
            "redacted blob",
            "alice",
            TestCluster::create_request(&env.c1, &[("w/red.txt", b"red\n")], &[], cid(via, 14)),
            ErrorCode::Redacted,
            None,
        ),
        (
            "unwritable path",
            "alice",
            TestCluster::create_request(&env.c1, &[], &["secrets/s.txt"], cid(via, 15)),
            ErrorCode::PermissionDenied,
            None,
        ),
    ];
    for (what, who, req, code, sub) in cases {
        let c = env.client(via, who).await;
        err(
            c.write().create_changeset(req).await,
            code,
            sub,
            t,
            &format!("CreateChangeset {what}"),
        );
    }
    let ErrorPayload::Limit { name, .. } = err(
        alice
            .write()
            .create_changeset({
                let mut r = TestCluster::create_request(&env.c1, &[(&path, &content)], &[], 1);
                r.meta_fields.as_mut().unwrap().message = vec![b'y'; 1_048_577];
                r
            })
            .await,
        ErrorCode::TooLarge,
        None,
        t,
        "CreateChangeset limit payload",
    )
    .payload
    else {
        panic!("TOO_LARGE carries a Limit payload")
    };
    assert_eq!(name, "max_message_bytes");
    env.unknown_fields_ok(via, "CreateChangeset", req.encode_to_vec(), "alice")
        .await;
}

/// A changeset on `parent` putting one file, created directly (setup for write cases).
async fn put(env: &Env, parent: &ObjectId, path: &str, content: &[u8], change: u32) -> ObjectId {
    let admin = env.client(Via::Server, "admin").await;
    TestCluster::commit(&admin, parent, &[(path, content)], &[], change).await
}

async fn merge_cs(env: &Env, a: &ObjectId, b: &ObjectId, change: u32) -> ObjectId {
    let admin = env.client(Via::Server, "admin").await;
    let r = admin
        .write()
        .merge(v1::MergeRequest {
            a: idb(a),
            b: idb(b),
            meta_fields: Some(TestCluster::meta("setup merge")),
            change_id: TestCluster::change_id(change),
        })
        .await
        .unwrap()
        .into_inner();
    ObjectId::from_bytes(&r.changeset_id).unwrap()
}

async fn case_rebase(env: &Env, via: Via) {
    let t = tier_of(via, "Rebase");
    let alice = env.client(via, "alice").await;
    let p = |n: &str| format!("w/{}/rebase-{n}.txt", via.as_str());
    let base = put(env, &env.c1, &p("base"), b"1\n2\n3\n", cid(via, 20)).await;
    let ours = put(env, &base, &p("base"), b"1\nours\n3\n", cid(via, 21)).await;
    let theirs = put(env, &base, &p("base"), b"1\ntheirs\n3\n", cid(via, 22)).await;
    let other = put(env, &base, &p("other"), b"o\n", cid(via, 23)).await;
    let same = put(env, &base, &p("base"), b"1\nours\n3\n", cid(via, 24)).await;
    let req = |c: &ObjectId, onto: &ObjectId| v1::RebaseRequest {
        changeset: idb(c),
        onto: idb(onto),
    };
    let r = ok(alice.write().rebase(req(&other, &ours)).await, t, "Rebase");
    let r2 = ok(alice.write().rebase(req(&other, &ours)).await, t, "Rebase deterministic");
    assert_eq!(r.changeset_id, r2.changeset_id);
    let e = err(
        alice.write().rebase(req(&theirs, &ours)).await,
        ErrorCode::MergeConflict,
        None,
        t,
        "Rebase conflict",
    );
    let ErrorPayload::MergeConflict(report) = e.payload else {
        panic!("MERGE_CONFLICT carries a conflict report")
    };
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].path, p("base").into_bytes());
    err(
        alice.write().rebase(req(&same, &ours)).await,
        ErrorCode::EmptyChange,
        None,
        t,
        "Rebase empty",
    );
    let m = merge_cs(env, &ours, &other, cid(via, 25)).await;
    err(
        alice.write().rebase(req(&m, &theirs)).await,
        ErrorCode::InvalidArgument,
        Some(subcode::MULTI_PARENT),
        t,
        "Rebase multi-parent",
    );
    err(
        alice
            .write()
            .rebase(req(&ObjectId::hash_encoded(Kind::Changeset, b"r"), &ours))
            .await,
        ErrorCode::NotFound,
        None,
        t,
        "Rebase missing",
    );
    err(
        env.client(via, "mallory").await.write().rebase(req(&other, &ours)).await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "Rebase without repository read",
    );
    env.unknown_fields_ok(via, "Rebase", req(&other, &ours).encode_to_vec(), "alice")
        .await;
}

async fn case_merge(env: &Env, via: Via) {
    let t = tier_of(via, "Merge");
    let alice = env.client(via, "alice").await;
    let p = |n: &str| format!("w/{}/merge-{n}.txt", via.as_str());
    let base = put(env, &env.c1, &p("base"), b"1\n2\n3\n", cid(via, 30)).await;
    let x = put(env, &base, &p("x"), b"x\n", cid(via, 31)).await;
    let y = put(env, &base, &p("y"), b"y\n", cid(via, 32)).await;
    let ours = put(env, &base, &p("base"), b"1\nours\n3\n", cid(via, 33)).await;
    let theirs = put(env, &base, &p("base"), b"1\ntheirs\n3\n", cid(via, 34)).await;
    let req = |a: &ObjectId, b: &ObjectId, n: u32| v1::MergeRequest {
        a: idb(a),
        b: idb(b),
        meta_fields: Some(TestCluster::meta("merge")),
        change_id: TestCluster::change_id(cid(via, n)),
    };
    ok(alice.write().merge(req(&x, &y, 35)).await, t, "Merge");
    let e = err(
        alice.write().merge(req(&ours, &theirs, 36)).await,
        ErrorCode::MergeConflict,
        None,
        t,
        "Merge conflict",
    );
    assert!(matches!(e.payload, ErrorPayload::MergeConflict(_)));
    // Criss-cross: two best common ancestors.
    let m1 = merge_cs(env, &x, &y, cid(via, 37)).await;
    let m2 = merge_cs(env, &y, &x, cid(via, 38)).await;
    err(
        alice.write().merge(req(&m1, &m2, 39)).await,
        ErrorCode::AmbiguousMergeBase,
        None,
        t,
        "Merge ambiguous base",
    );
    err(
        alice
            .write()
            .merge(req(&x, &ObjectId::hash_encoded(Kind::Changeset, b"mm"), 40))
            .await,
        ErrorCode::NotFound,
        None,
        t,
        "Merge missing",
    );
    let mut no_meta = req(&x, &y, 41);
    no_meta.meta_fields = None;
    err(
        alice.write().merge(no_meta).await,
        ErrorCode::InvalidArgument,
        Some(subcode::MISSING_FIELD),
        t,
        "Merge missing meta",
    );
    // A merge whose result touches /secrets needs write there.
    let sec = put(env, &base, "secrets/merge-in.txt", &uniq("sec", via, 1), cid(via, 42)).await;
    err(
        alice.write().merge(req(&x, &sec, 43)).await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "Merge into unwritable path",
    );
    env.unknown_fields_ok(via, "Merge", req(&x, &y, 35).encode_to_vec(), "alice")
        .await;
}

async fn case_land(env: &Env, via: Via) {
    let t = tier_of(via, "Land");
    let alice = env.client(via, "alice").await;
    let p = |n: &str| format!("w/{}/land-{n}.txt", via.as_str());
    let main = env.main().await;
    let a = put(env, &main, &p("a"), b"a\n", cid(via, 50)).await;
    let r = ok(
        alice
            .write()
            .land(v1::LandRequest {
                r#ref: "main".into(),
                head: idb(&a),
                allow_empty: false,
            })
            .await,
        t,
        "Land",
    );
    assert_eq!(r.landed.len(), 1);
    // Retry: the recorded result (SPEC 16.1 class 5).
    let again = land(env, via, "alice", &a).await.unwrap();
    assert_eq!((again.target, again.version), (r.target.clone(), r.version));

    // ALREADY_LANDED: same change id, different source.
    let main = env.main().await;
    let dup = put(env, &main, &p("a"), b"different\n", cid(via, 50)).await;
    let e = check_err(
        &land(env, via, "alice", &dup).await.unwrap_err(),
        ErrorCode::AlreadyLanded,
        None,
        t,
        "Land already landed",
    );
    assert!(matches!(e.payload, ErrorPayload::AlreadyLanded { .. }));
    // EMPTY_CHANGE: the change is already on main under another change id.
    let stale = put(env, &a, &p("b"), b"b\n", cid(via, 52)).await;
    land(env, via, "alice", &stale).await.unwrap();
    let redo = put(env, &a, &p("b"), b"b\n", cid(via, 53)).await;
    check_err(
        &land(env, via, "alice", &redo).await.unwrap_err(),
        ErrorCode::EmptyChange,
        None,
        t,
        "Land empty change",
    );
    // MERGE_CONFLICT against main.
    let conflict = put(env, &a, &p("b"), b"conflicting b\n", cid(via, 54)).await;
    check_err(
        &land(env, via, "alice", &conflict).await.unwrap_err(),
        ErrorCode::MergeConflict,
        None,
        t,
        "Land conflict",
    );
    // A stack containing a merge.
    let main = env.main().await;
    let x = put(env, &main, &p("x"), b"x\n", cid(via, 55)).await;
    let y = put(env, &main, &p("y"), b"y\n", cid(via, 56)).await;
    let m = merge_cs(env, &x, &y, cid(via, 57)).await;
    check_err(
        &land(env, via, "alice", &m).await.unwrap_err(),
        ErrorCode::InvalidArgument,
        Some(subcode::MERGE_IN_STACK),
        t,
        "Land merge in stack",
    );
    // PERMISSION_DENIED: no update on the ref.
    let z = put(env, &env.main().await, &p("z"), b"z\n", cid(via, 58)).await;
    check_err(
        &land(env, via, "mallory", &z).await.unwrap_err(),
        ErrorCode::PermissionDenied,
        None,
        t,
        "Land without update",
    );
    // PERMISSION_DENIED: a member writes /secrets.
    let s = put(
        env,
        &env.main().await,
        &format!("secrets/{}-land.txt", via.as_str()),
        b"s\n",
        cid(via, 59),
    )
    .await;
    check_err(
        &land(env, via, "alice", &s).await.unwrap_err(),
        ErrorCode::PermissionDenied,
        None,
        t,
        "Land unwritable member",
    );
    check_err(
        &land(env, via, "alice", &ObjectId::hash_encoded(Kind::Changeset, b"l"))
            .await
            .unwrap_err(),
        ErrorCode::NotFound,
        None,
        t,
        "Land missing head",
    );
    let body = v1::LandRequest {
        r#ref: "main".into(),
        head: idb(&a),
        allow_empty: false,
    }
    .encode_to_vec();
    env.unknown_fields_ok(via, "Land", body, "alice").await;
}

// ---------------------------------------------------------------------------------------
// Cases: RefService.
// ---------------------------------------------------------------------------------------

async fn create_ref(env: &Env, name: &str, target: &ObjectId) -> u64 {
    let admin = env.client(Via::Server, "admin").await;
    admin
        .refs()
        .create_ref(v1::CreateRefRequest {
            name: name.into(),
            target: idb(target),
            reason: "setup".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .version
}

fn update_req(name: &str, v: u64, target: &ObjectId, force: bool) -> v1::UpdateRefRequest {
    v1::UpdateRefRequest {
        name: name.into(),
        expected_version: v,
        new_target: idb(target),
        allow_non_fast_forward: force,
    }
}

async fn case_update_ref(env: &Env, via: Via) {
    let t = tier_of(via, "UpdateRef");
    let alice = env.client(via, "alice").await;
    let name = format!("upd-{}", via.as_str());
    create_ref(env, &name, &env.c1).await;
    let next = put(env, &env.c1, &format!("w/{name}.txt"), b"u\n", cid(via, 60)).await;
    let next2 = put(env, &next, &format!("w/{name}.txt"), b"u2\n", cid(via, 61)).await;
    let r = ok(
        alice.refs().update_ref(update_req(&name, 1, &next, false)).await,
        t,
        "UpdateRef",
    );
    assert_eq!(r.version, 2);
    let e = err(
        alice.refs().update_ref(update_req(&name, 1, &next2, false)).await,
        ErrorCode::RefVersionMismatch,
        None,
        t,
        "UpdateRef stale version",
    );
    assert_eq!(
        e.payload,
        ErrorPayload::RefMismatch {
            current_version: 2,
            current_target: Some(next)
        }
    );
    err(
        alice.refs().update_ref(update_req(&name, 2, &env.c1, false)).await,
        ErrorCode::NotFastForward,
        None,
        t,
        "UpdateRef non-fast-forward",
    );
    let admin = env.client(via, "admin").await;
    let r = ok(
        admin.refs().update_ref(update_req(&name, 2, &env.c1, true)).await,
        t,
        "UpdateRef forced",
    );
    assert_eq!(r.version, 3);
    err(
        env.client(via, "mallory")
            .await
            .refs()
            .update_ref(update_req(&name, 3, &next, false))
            .await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "UpdateRef without update",
    );
    err(
        alice
            .refs()
            .update_ref(update_req("bad..name", 1, &next, false))
            .await,
        ErrorCode::InvalidArgument,
        Some(subcode::INVALID_REF_NAME),
        t,
        "UpdateRef malformed name",
    );
    err(
        alice
            .refs()
            .update_ref(update_req(
                &name,
                3,
                &ObjectId::hash_encoded(Kind::Changeset, b"u"),
                false,
            ))
            .await,
        ErrorCode::NotFound,
        None,
        t,
        "UpdateRef missing target",
    );
    env.unknown_fields_ok(
        via,
        "UpdateRef",
        update_req(&name, 3, &next, false).encode_to_vec(),
        "alice",
    )
    .await;
}

async fn case_create_ref(env: &Env, via: Via) {
    let t = tier_of(via, "CreateRef");
    let admin = env.client(via, "admin").await;
    let req = |n: &str, target: &ObjectId| v1::CreateRefRequest {
        name: n.to_owned(),
        target: idb(target),
        reason: "contract".into(),
    };
    let name = format!("cr-{}", via.as_str());
    let r = ok(admin.refs().create_ref(req(&name, &env.c1)).await, t, "CreateRef");
    assert_eq!(r.version, 1);
    err(
        admin.refs().create_ref(req(&name, &env.c1)).await,
        ErrorCode::InvalidArgument,
        Some(subcode::REF_EXISTS),
        t,
        "CreateRef existing",
    );
    err(
        admin
            .refs()
            .create_ref(req(
                &format!("{name}-x"),
                &ObjectId::hash_encoded(Kind::Changeset, b"c"),
            ))
            .await,
        ErrorCode::NotFound,
        None,
        t,
        "CreateRef missing target",
    );
    err(
        env.client(via, "alice")
            .await
            .refs()
            .create_ref(req(&format!("{name}-y"), &env.c1))
            .await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "CreateRef without admin.refs",
    );
    err(
        admin.refs().create_ref(req("bad..ref", &env.c1)).await,
        ErrorCode::InvalidArgument,
        Some(subcode::INVALID_REF_NAME),
        t,
        "CreateRef malformed name",
    );
    env.unknown_fields_ok(
        via,
        "CreateRef",
        req(&format!("{name}-u"), &env.c1).encode_to_vec(),
        "admin",
    )
    .await;
}

async fn case_delete_ref(env: &Env, via: Via) {
    let t = tier_of(via, "DeleteRef");
    let admin = env.client(via, "admin").await;
    let req = |n: &str, v: u64| v1::DeleteRefRequest {
        name: n.to_owned(),
        expected_version: v,
        reason: "contract".into(),
    };
    let name = format!("del-{}", via.as_str());
    create_ref(env, &name, &env.c1).await;
    let e = err(
        admin.refs().delete_ref(req(&name, 7)).await,
        ErrorCode::RefVersionMismatch,
        None,
        t,
        "DeleteRef stale version",
    );
    assert!(matches!(
        e.payload,
        ErrorPayload::RefMismatch {
            current_version: 1,
            ..
        }
    ));
    err(
        env.client(via, "alice")
            .await
            .refs()
            .delete_ref(req(&name, 1))
            .await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "DeleteRef without admin.refs",
    );
    ok(admin.refs().delete_ref(req(&name, 1)).await, t, "DeleteRef");
    err(
        admin.refs().delete_ref(req(&name, 2)).await,
        ErrorCode::NotFound,
        None,
        t,
        "DeleteRef absent",
    );
    let name2 = format!("del2-{}", via.as_str());
    create_ref(env, &name2, &env.c1).await;
    env.unknown_fields_ok(via, "DeleteRef", req(&name2, 1).encode_to_vec(), "admin")
        .await;
}

async fn case_get_ref_log(env: &Env, via: Via) {
    let t = tier_of(via, "GetRefLog");
    let alice = env.client(via, "alice").await;
    let req = |n: &str| v1::GetRefLogRequest {
        name: n.to_owned(),
        from_version: 1,
        limit: 2,
    };
    let r = ok(alice.refs().get_ref_log(req("main")).await, t, "GetRefLog");
    assert_eq!(r.entries.len(), 2);
    assert_eq!(r.entries[0].version, 1);
    assert_eq!(r.entries[1].new_target, idb(&env.c1));
    let r = ok(alice.refs().get_ref_log(req("never-existed")).await, t, "GetRefLog empty");
    assert!(r.entries.is_empty());
    err(
        env.client(via, "mallory")
            .await
            .refs()
            .get_ref_log(req("main"))
            .await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "GetRefLog without repository read",
    );
    err(
        alice.refs().get_ref_log(req("bad..name")).await,
        ErrorCode::InvalidArgument,
        Some(subcode::INVALID_REF_NAME),
        t,
        "GetRefLog malformed name",
    );
    env.unknown_fields_ok(via, "GetRefLog", req("main").encode_to_vec(), "alice")
        .await;
}

// ---------------------------------------------------------------------------------------
// Cases: AuthService and AdminService.
// ---------------------------------------------------------------------------------------

fn service_login(name: &str, secret: &str) -> v1::LoginRequest {
    v1::LoginRequest {
        credential: Some(v1::login_request::Credential::Service(
            v1::ServiceCredential {
                service_name: name.into(),
                service_secret: secret.into(),
            },
        )),
    }
}

fn github_login(token: &str) -> v1::LoginRequest {
    v1::LoginRequest {
        credential: Some(v1::login_request::Credential::GithubAccessToken(
            token.into(),
        )),
    }
}

async fn case_login(env: &Env, via: Via) {
    let t = tier_of(via, "Login");
    let anon = env.c.client_with_token(via, None).await;
    let (name, secret) = TestCluster::SERVICES[0];
    let r = ok(anon.auth().login(service_login(name, secret)).await, t, "Login service");
    assert_eq!(r.principal, format!("service:{name}"));
    env.c.github.add_user("gho_contract", "alice", 7001);
    let r = ok(anon.auth().login(github_login("gho_contract")).await, t, "Login github");
    assert_eq!(r.principal, "github:alice");
    for (what, req) in [
        ("Login wrong secret", service_login(name, "wrong")),
        ("Login unknown service", service_login("nobody", secret)),
        ("Login bad github token", github_login("gho_not_a_user")),
    ] {
        err(anon.auth().login(req).await, ErrorCode::Unauthenticated, None, t, what);
    }
    err(
        anon.auth().login(v1::LoginRequest { credential: None }).await,
        ErrorCode::InvalidArgument,
        Some(subcode::MISSING_FIELD),
        t,
        "Login without credential",
    );
    env.c.github.set_down(true);
    err(
        anon.auth().login(github_login("gho_contract")).await,
        ErrorCode::Unavailable,
        None,
        t,
        "Login while GitHub is down",
    );
    env.c.github.set_down(false);
    env.unknown_fields_ok(via, "Login", service_login(name, secret).encode_to_vec(), "-")
        .await;
}

async fn case_who_am_i(env: &Env, via: Via) {
    let t = tier_of(via, "WhoAmI");
    let r = ok(
        env.client(via, "carol").await.auth().who_am_i(v1::WhoAmIRequest {}).await,
        t,
        "WhoAmI",
    );
    assert_eq!(r.principal, "github:carol");
    assert_eq!(r.groups, vec!["eng".to_owned(), "security".to_owned()]);
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(r.expires_at_unix > now);
    env.unknown_fields_ok(via, "WhoAmI", Vec::new(), "alice").await;
}

async fn case_check_access(env: &Env, via: Via) {
    let t = tier_of(via, "CheckAccess");
    let caller = env.client(via, "mallory").await;
    let ids = vec![
        idb(&ObjectId::hash_blob(b"readme\n")),
        idb(&ObjectId::hash_blob(b"secret\n")),
    ];
    let req = |subject: String, ids: Vec<Vec<u8>>| v1::CheckAccessRequest {
        subject_token: subject,
        object_ids: ids,
    };
    let r = ok(
        caller
            .auth()
            .check_access(req(env.c.token_for(&gh("alice")), ids.clone()))
            .await,
        t,
        "CheckAccess",
    );
    assert_eq!(r.allowed, vec![true, false]);
    assert_eq!(
        r.access_class,
        vec![
            v1::AccessClass::Root as i32,
            v1::AccessClass::Unspecified as i32
        ]
    );
    err(
        caller.auth().check_access(req("garbage".into(), ids.clone())).await,
        ErrorCode::Unauthenticated,
        None,
        t,
        "CheckAccess invalid subject",
    );
    err(
        caller.auth().check_access(req(String::new(), ids.clone())).await,
        ErrorCode::InvalidArgument,
        Some(subcode::MISSING_FIELD),
        t,
        "CheckAccess missing subject",
    );
    err(
        caller
            .auth()
            .check_access(req(env.c.token_for(&gh("alice")), vec![vec![4; 4]]))
            .await,
        ErrorCode::InvalidArgument,
        None,
        t,
        "CheckAccess malformed id",
    );
    env.unknown_fields_ok(
        via,
        "CheckAccess",
        req(env.c.token_for(&gh("alice")), ids).encode_to_vec(),
        "mallory",
    )
    .await;
}

/// A fresh blob referenced at a public path (redaction target).
async fn redactable(env: &Env, via: Via, n: u32) -> ObjectId {
    let content = uniq("redactable", via, n);
    let main = env.main().await;
    put(
        env,
        &main,
        &format!("w/{}/redactable-{n}.txt", via.as_str()),
        &content,
        cid(via, 100 + n),
    )
    .await;
    ObjectId::hash_blob(&content)
}

fn redact_req(ids: &[ObjectId]) -> v1::RedactRequest {
    v1::RedactRequest {
        object_ids: ids.iter().map(idb).collect(),
        reason: "contract".into(),
        purge_after_seconds: None,
    }
}

async fn case_redact(env: &Env, via: Via) {
    let t = tier_of(via, "Redact");
    let admin = env.client(via, "admin").await;
    let id = redactable(env, via, 1).await;
    let before = env.c.servers[0].ctx().redactions.epoch();
    let r = ok(admin.admin().redact(redact_req(&[id])).await, t, "Redact");
    assert!(r.epoch > before);
    err(
        env.client(via, "alice").await.admin().redact(redact_req(&[id])).await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "Redact without admin.redact",
    );
    let tree = {
        let alice = env.client(Via::Server, "alice").await;
        let l = alice.query().list_tree(list_req(&env.c1, "docs")).await.unwrap();
        ObjectId::from_bytes(&l.into_inner().tree_id).unwrap()
    };
    err(
        admin.admin().redact(redact_req(&[tree])).await,
        ErrorCode::InvalidArgument,
        Some(subcode::NOT_REDACTABLE),
        t,
        "Redact a tree",
    );
    err(
        admin
            .admin()
            .redact(redact_req(&[ObjectId::hash_blob(b"unknown to the server")]))
            .await,
        ErrorCode::NotFound,
        None,
        t,
        "Redact unknown id",
    );
    err(
        admin.admin().redact(redact_req(&[])).await,
        ErrorCode::InvalidArgument,
        Some(subcode::MISSING_FIELD),
        t,
        "Redact without ids",
    );
    let id2 = redactable(env, via, 2).await;
    env.unknown_fields_ok(via, "Redact", redact_req(&[id2]).encode_to_vec(), "admin")
        .await;
}

async fn case_unredact(env: &Env, via: Via) {
    let t = tier_of(via, "Unredact");
    let admin = env.client(via, "admin").await;
    let id = redactable(env, via, 3).await;
    let live = redactable(env, via, 4).await;
    admin.admin().redact(redact_req(&[id])).await.unwrap();
    let req = |ids: &[ObjectId]| v1::UnredactRequest {
        object_ids: ids.iter().map(idb).collect(),
    };
    err(
        env.client(via, "alice").await.admin().unredact(req(&[id])).await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "Unredact without admin.redact",
    );
    ok(admin.admin().unredact(req(&[id])).await, t, "Unredact");
    err(
        admin.admin().unredact(req(&[live])).await,
        ErrorCode::InvalidArgument,
        Some(subcode::NOT_QUARANTINED),
        t,
        "Unredact a live object",
    );
    admin.admin().redact(redact_req(&[id])).await.unwrap();
    env.unknown_fields_ok(via, "Unredact", req(&[id]).encode_to_vec(), "admin")
        .await;
}

async fn case_purge_now(env: &Env, via: Via) {
    let t = tier_of(via, "PurgeNow");
    let admin = env.client(via, "admin").await;
    let id = redactable(env, via, 5).await;
    let live = redactable(env, via, 6).await;
    admin.admin().redact(redact_req(&[id])).await.unwrap();
    let req = |ids: &[ObjectId]| v1::PurgeNowRequest {
        object_ids: ids.iter().map(idb).collect(),
        reason: "contract".into(),
    };
    err(
        env.client(via, "alice").await.admin().purge_now(req(&[id])).await,
        ErrorCode::PermissionDenied,
        None,
        t,
        "PurgeNow without admin.purge",
    );
    ok(admin.admin().purge_now(req(&[id])).await, t, "PurgeNow");
    err(
        admin.admin().purge_now(req(&[live])).await,
        ErrorCode::InvalidArgument,
        Some(subcode::NOT_QUARANTINED),
        t,
        "PurgeNow a live object",
    );
    err(
        admin
            .admin()
            .purge_now(req(&[ObjectId::hash_blob(b"no such object for purge")]))
            .await,
        ErrorCode::NotFound,
        None,
        t,
        "PurgeNow unknown id",
    );
    // Already purged ids are skipped (idempotent).
    env.unknown_fields_ok(via, "PurgeNow", req(&[id]).encode_to_vec(), "admin")
        .await;
}

async fn case_list_redactions(env: &Env, via: Via) {
    let t = tier_of(via, "ListRedactions");
    let r = ok(
        env.client(via, "mallory")
            .await
            .admin()
            .list_redactions(v1::ListRedactionsRequest { since_epoch: 0 })
            .await,
        t,
        "ListRedactions",
    );
    assert!(r.entries.iter().any(|e| e.object_id == idb(&env.red)));
    assert!(r.entries.iter().all(|e| e.reason.is_empty()), "reasons only for admins");
    let body = v1::ListRedactionsRequest { since_epoch: 0 }.encode_to_vec();
    env.unknown_fields_ok(via, "ListRedactions", body, "alice").await;
}

// ---------------------------------------------------------------------------------------
// The descriptor-driven driver.
// ---------------------------------------------------------------------------------------

/// Dispatch one RPC to its contract case; `false` when the RPC has no case.
async fn run_case(env: &Env, method: &str, via: Via) -> bool {
    match method {
        "GetServerInfo" => case_get_server_info(env, via).await,
        "GetObjects" => case_get_objects(env, via).await,
        "HasObjects" => case_has_objects(env, via).await,
        "ResolveRef" => case_resolve_ref(env, via).await,
        "GetChangesets" => case_get_changesets(env, via).await,
        "ReadPath" => case_read_path(env, via).await,
        "ListTree" => case_list_tree(env, via).await,
        "Log" => case_log(env, via).await,
        "Blame" => case_blame(env, via).await,
        "Diff" => case_diff(env, via).await,
        "IsAncestor" => case_is_ancestor(env, via).await,
        "MergeBase" => case_merge_base(env, via).await,
        "Resolve" => case_resolve(env, via).await,
        "GetDerived" => case_get_derived(env, via).await,
        "UploadObjects" => case_upload_objects(env, via).await,
        "CreateChangeset" => case_create_changeset(env, via).await,
        "Rebase" => case_rebase(env, via).await,
        "Merge" => case_merge(env, via).await,
        "Land" => case_land(env, via).await,
        "UpdateRef" => case_update_ref(env, via).await,
        "CreateRef" => case_create_ref(env, via).await,
        "DeleteRef" => case_delete_ref(env, via).await,
        "GetRefLog" => case_get_ref_log(env, via).await,
        "Login" => case_login(env, via).await,
        "WhoAmI" => case_who_am_i(env, via).await,
        "CheckAccess" => case_check_access(env, via).await,
        "Redact" => case_redact(env, via).await,
        "Unredact" => case_unredact(env, via).await,
        "PurgeNow" => case_purge_now(env, via).await,
        "ListRedactions" => case_list_redactions(env, via).await,
        _ => return false,
    }
    true
}

/// The RPCs whose request has required fields (`// required` in the proto, or semantically
/// required by the handler contract): an all-default request is INVALID_ARGUMENT/
/// missing_field. The others accept an empty request.
const REQUIRED_FIELDS: &[&str] = &[
    "ResolveRef",
    "ReadPath",
    "ListTree",
    "Log",
    "Blame",
    "Diff",
    "IsAncestor",
    "MergeBase",
    "Resolve",
    "GetDerived",
    "CreateChangeset",
    "Rebase",
    "Merge",
    "Land",
    "UpdateRef",
    "CreateRef",
    "DeleteRef",
    "GetRefLog",
    "Login",
    "CheckAccess",
    "Redact",
    "Unredact",
    "PurgeNow",
];

/// Run the matrix for the RPCs of `service` (all services when `None`).
async fn run_matrix(backend: Backend, service: Option<&str>) {
    let env = Env::start(backend).await;
    let rpcs: Vec<_> = TestCluster::rpcs()
        .into_iter()
        .filter(|r| service.is_none_or(|s| r.service == s))
        .collect();
    assert!(!rpcs.is_empty());
    for rpc in &rpcs {
        for via in Via::ALL {
            assert!(
                run_case(&env, &rpc.method, via).await,
                "{} has no contract case",
                rpc.path
            );
        }
    }
    env.sync_daemon().await;
    invariants::check_16_2(&env.c).await.unwrap();
}

#[test]
fn every_descriptor_rpc_has_a_contract_case() {
    // `run_case` must know every method; checked without running it by matching names.
    let known = [
        "GetServerInfo",
        "GetObjects",
        "HasObjects",
        "ResolveRef",
        "GetChangesets",
        "ReadPath",
        "ListTree",
        "Log",
        "Blame",
        "Diff",
        "IsAncestor",
        "MergeBase",
        "Resolve",
        "GetDerived",
        "UploadObjects",
        "CreateChangeset",
        "Rebase",
        "Merge",
        "Land",
        "UpdateRef",
        "CreateRef",
        "DeleteRef",
        "GetRefLog",
        "Login",
        "WhoAmI",
        "CheckAccess",
        "Redact",
        "Unredact",
        "PurgeNow",
        "ListRedactions",
    ];
    let rpcs = TestCluster::rpcs();
    for r in &rpcs {
        assert!(known.contains(&r.method.as_str()), "{} has no contract case", r.path);
    }
    assert_eq!(rpcs.len(), known.len(), "a case names an RPC the descriptor lacks");
}

macro_rules! matrix_tests {
    ($($name:ident, $ext:ident, $svc:expr;)*) => {$(
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            run_matrix(Backend::Memory, Some($svc)).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn $ext() {
            if skip_external() {
                return;
            }
            run_matrix(Backend::External, Some($svc)).await;
        }
    )*};
}

matrix_tests! {
    contract_matrix_object_service, external_contract_matrix_object_service, "ObjectService";
    contract_matrix_query_service, external_contract_matrix_query_service, "QueryService";
    contract_matrix_write_service, external_contract_matrix_write_service, "WriteService";
    contract_matrix_ref_service, external_contract_matrix_ref_service, "RefService";
    contract_matrix_auth_service, external_contract_matrix_auth_service, "AuthService";
    contract_matrix_admin_service, external_contract_matrix_admin_service, "AdminService";
}

// ---------------------------------------------------------------------------------------
// Generic sections: authentication failure, missing required fields, rate limiting,
// unavailability.
// ---------------------------------------------------------------------------------------

/// Flip one character of the token's signed payload.
fn tamper(token: &str) -> String {
    let mut b = token.as_bytes().to_vec();
    let i = b.len() / 2;
    b[i] = if b[i] == b'A' { b'B' } else { b'A' };
    String::from_utf8(b).unwrap()
}

async fn run_auth_failures(backend: Backend) {
    let env = Env::start(backend).await;
    let srv = &env.c.servers[0];
    let alice = gh("alice");
    let expired = srv
        .keyring
        .issue(
            &alice,
            Duration::from_secs(60),
            SystemTime::now() - Duration::from_secs(3600),
        )
        .unwrap()
        .0;
    let wrong_key = scion_auth::Keyring::generate_for_tests(1)
        .issue(&alice, Duration::from_secs(3600), SystemTime::now())
        .unwrap()
        .0;
    let tampered = tamper(&env.c.token_for(&alice));
    let bad_tokens: Vec<(&str, Option<String>)> = vec![
        ("missing", None),
        ("malformed", Some("garbage".into())),
        ("expired", Some(expired)),
        ("wrong key", Some(wrong_key)),
        ("tampered", Some(tampered)),
    ];
    let open = ["GetServerInfo", "Login"];
    for via in Via::ALL {
        let channel = env.c.client_with_token(via, None).await.channel();
        for rpc in TestCluster::rpcs() {
            let t = tier_of(via, &rpc.method);
            for (what, token) in &bad_tokens {
                let (status, md) =
                    TestCluster::raw_call(channel.clone(), &rpc, None, token.as_deref()).await;
                let label = format!("{} via {} ({what} token)", rpc.path, via.as_str());
                check_trailers(&md, t, &label);
                if open.contains(&rpc.method.as_str()) {
                    assert_ne!(status.code(), tonic::Code::Unauthenticated, "{label}");
                } else {
                    check_err(&status, ErrorCode::Unauthenticated, None, t, &label);
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_rpc_rejects_missing_and_invalid_tokens() {
    run_auth_failures(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_every_rpc_rejects_missing_and_invalid_tokens() {
    if skip_external() {
        return;
    }
    run_auth_failures(Backend::External).await;
}

async fn run_missing_fields(backend: Backend) {
    let env = Env::start(backend).await;
    let token = env.c.token_for(&gh("admin"));
    for via in Via::ALL {
        let channel = env.c.client_with_token(via, None).await.channel();
        for rpc in TestCluster::rpcs() {
            let t = tier_of(via, &rpc.method);
            let label = format!("{} via {} (empty request)", rpc.path, via.as_str());
            let body = (!rpc.client_streaming).then(Bytes::new);
            let (status, md) =
                TestCluster::raw_call(channel.clone(), &rpc, body, Some(&token)).await;
            check_trailers(&md, t, &label);
            if REQUIRED_FIELDS.contains(&rpc.method.as_str()) {
                check_err(
                    &status,
                    ErrorCode::InvalidArgument,
                    Some(subcode::MISSING_FIELD),
                    t,
                    &label,
                );
            } else {
                assert_eq!(status.code(), tonic::Code::Ok, "{label}: {status:?}");
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_required_fields_are_invalid_argument() {
    run_missing_fields(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_missing_required_fields_are_invalid_argument() {
    if skip_external() {
        return;
    }
    run_missing_fields(Backend::External).await;
}

async fn run_rate_limit(backend: Backend) {
    let mut o = ClusterOpts::new(backend);
    o.server.tweak = Some(Arc::new(|c| {
        c.rate_limit.per_principal_rps = 1;
        c.rate_limit.burst = 2;
    }));
    let c = TestCluster::start(o).await;
    for via in Via::ALL {
        let who = format!("rl-{}", via.as_str());
        let client = c.client(via, &gh(&who)).await;
        let mut limited = None;
        for _ in 0..20 {
            if let Err(s) = client.auth().who_am_i(v1::WhoAmIRequest {}).await {
                limited = Some(s);
                break;
            }
        }
        let s = limited.unwrap_or_else(|| panic!("never rate limited via {}", via.as_str()));
        check_err(
            &s,
            ErrorCode::ResourceExhausted,
            None,
            Tier::Server,
            &format!("rate limit via {}", via.as_str()),
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limited_principals_get_resource_exhausted() {
    run_rate_limit(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_rate_limited_principals_get_resource_exhausted() {
    if skip_external() {
        return;
    }
    run_rate_limit(Backend::External).await;
}

async fn run_unavailable(backend: Backend) {
    let env = Env::start(backend).await;
    let faults = env.c.faults();
    for via in Via::ALL {
        let alice = env.client(via, "alice").await;
        let fresh = format!("fresh-{}", via.as_str());
        faults.arm(
            "meta.get_ref.before",
            FaultAction::Error(ErrorCode::Unavailable),
            None,
        );
        let r = alice
            .objects()
            .resolve_ref(v1::ResolveRefRequest {
                name: fresh,
                at_version: None,
            })
            .await;
        faults.disarm_all();
        err(
            r,
            ErrorCode::Unavailable,
            None,
            tier_of(via, "ResolveRef"),
            &format!("ResolveRef with the metadata store down via {}", via.as_str()),
        );
        faults.arm(
            "meta.commit_changeset.before",
            FaultAction::Error(ErrorCode::Unavailable),
            None,
        );
        let content = uniq("unavailable", via, 1);
        TestCluster::upload(&alice, &[&content]).await;
        let r = alice
            .write()
            .create_changeset(TestCluster::create_request(
                &env.c1,
                &[("w/unavailable.txt", &content)],
                &[],
                cid(via, 200),
            ))
            .await;
        faults.disarm_all();
        err(
            r,
            ErrorCode::Unavailable,
            None,
            Tier::Server,
            &format!("CreateChangeset with the metadata store down via {}", via.as_str()),
        );
    }
    invariants::check_16_2(&env.c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_outages_are_unavailable_and_retryable() {
    run_unavailable(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_storage_outages_are_unavailable_and_retryable() {
    if skip_external() {
        return;
    }
    run_unavailable(Backend::External).await;
}
