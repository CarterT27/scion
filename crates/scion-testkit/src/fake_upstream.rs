//! In-memory `scion.v1` upstream for daemon and client tests (PLAN 2.21; P08).
//!
//! [`FakeUpstream`] serves, from in-memory maps:
//! - ObjectService (SPEC 7.3): GetServerInfo (configured public keys), GetObjects (per-item
//!   ok / not_found / denied / redacted, raw or zstd frames, offset/length ranges, access class,
//!   multi-chunk items, a stream-kill knob), HasObjects, ResolveRef;
//! - QueryService GetChangesets and Resolve (SPEC 7.4);
//! - AuthService CheckAccess and WhoAmI, AdminService ListRedactions (SPEC 7.7).
//!
//! Every other method returns UNIMPLEMENTED. Every RPC except GetServerInfo requires a bearer
//! token that the configured `principal_of` closure maps to a principal. Responses carry the
//! SPEC 7.1 trailers through `EpochTrailerLayer` (tier `server`).
//!
//! With `tls: true` it listens on `127.0.0.1:0` with a [`TestTls`] certificate and its endpoint
//! is `https://localhost:<port>`. Without TLS it listens on a Unix socket in a private temp
//! directory, because [`Endpoint`] has no plain-TCP form (TLS is REQUIRED on TCP, SPEC 17).

use crate::TestTls;
use bytes::Bytes;
use parking_lot::Mutex;
use scion_core::{ErrorPayload, ObjectId, ScionError, Tier, subcode};
use scion_proto::v1;
use scion_proto::{
    Endpoint, EpochSource, EpochTrailerLayer, Epochs, TlsOptions, status_from_error,
};
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

/// Default size of the data in one GetObjects chunk.
const DEFAULT_CHUNK_SIZE: usize = 64 * 1024;
/// zstd level used for ZSTD frames.
const ZSTD_LEVEL: i32 = 3;

/// Full gRPC method paths, as recorded in [`RecordedRequest::method`].
pub mod methods {
    /// `ObjectService/GetServerInfo`.
    pub const GET_SERVER_INFO: &str = "/scion.v1.ObjectService/GetServerInfo";
    /// `ObjectService/GetObjects`.
    pub const GET_OBJECTS: &str = "/scion.v1.ObjectService/GetObjects";
    /// `ObjectService/HasObjects`.
    pub const HAS_OBJECTS: &str = "/scion.v1.ObjectService/HasObjects";
    /// `ObjectService/ResolveRef`.
    pub const RESOLVE_REF: &str = "/scion.v1.ObjectService/ResolveRef";
    /// `QueryService/GetChangesets`.
    pub const GET_CHANGESETS: &str = "/scion.v1.QueryService/GetChangesets";
    /// `QueryService/Resolve`.
    pub const RESOLVE: &str = "/scion.v1.QueryService/Resolve";
    /// `AuthService/CheckAccess`.
    pub const CHECK_ACCESS: &str = "/scion.v1.AuthService/CheckAccess";
    /// `AuthService/WhoAmI`.
    pub const WHO_AM_I: &str = "/scion.v1.AuthService/WhoAmI";
    /// `AdminService/ListRedactions`.
    pub const LIST_REDACTIONS: &str = "/scion.v1.AdminService/ListRedactions";
}

/// Options of a [`FakeUpstream`].
#[derive(Clone)]
pub struct FakeUpstreamOpts {
    /// Serve TLS (via [`crate::TestTls`]).
    pub tls: bool,
    /// Maps a bearer token to a principal id string (`None` = UNAUTHENTICATED).
    pub principal_of: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
    /// Token public keys published by GetServerInfo.
    pub public_keys: Vec<(String, [u8; 32])>,
}

impl Default for FakeUpstreamOpts {
    fn default() -> Self {
        FakeUpstreamOpts {
            tls: false,
            principal_of: Arc::new(|_| None),
            public_keys: Vec::new(),
        }
    }
}

/// One recorded request.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedRequest {
    /// Full gRPC method path.
    pub method: String,
    /// Object/changeset ids in the request.
    pub ids: Vec<ObjectId>,
    /// Caller principal, when authenticated.
    pub principal: Option<String>,
}

#[derive(Clone)]
struct StoredObject {
    raw: Bytes,
    restricted: bool,
}

struct State {
    objects: BTreeMap<ObjectId, StoredObject>,
    changesets: BTreeMap<ObjectId, Bytes>,
    refs: BTreeMap<String, BTreeMap<u64, ObjectId>>,
    denied: BTreeSet<(String, ObjectId)>,
    /// Redacted id -> redaction epoch at which it was listed.
    redacted: BTreeMap<ObjectId, u64>,
    corrupt: BTreeSet<ObjectId>,
    kill_after: Option<u64>,
    down: bool,
    policy_epoch: u64,
    redaction_epoch: u64,
    zstd: bool,
    chunk_size: usize,
    requests: Vec<RecordedRequest>,
}

struct Shared {
    state: Mutex<State>,
    principal_of: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
    public_keys: Vec<(String, [u8; 32])>,
}

impl EpochSource for Shared {
    fn epochs(&self) -> Epochs {
        let s = self.state.lock();
        Epochs {
            policy: s.policy_epoch,
            redaction: s.redaction_epoch,
            tier: Tier::Server,
        }
    }
}

/// ObjectService (+ GetChangesets/Resolve, CheckAccess/WhoAmI, ListRedactions) on
/// `127.0.0.1:0`; every other method is UNIMPLEMENTED.
pub struct FakeUpstream {
    shared: Arc<Shared>,
    endpoint: Endpoint,
    tls: Option<TlsOptions>,
    local_addr: Option<SocketAddr>,
    _dir: Option<tempfile::TempDir>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        if let Some(t) = &self.task {
            t.abort();
        }
    }
}

fn corrupted(raw: &Bytes) -> Bytes {
    let mut v = raw.to_vec();
    match v.first_mut() {
        Some(b) => *b ^= 0xff,
        None => v.push(0x5a),
    }
    Bytes::from(v)
}

fn split_chunks(data: &[u8], chunk: usize) -> Vec<(u64, Vec<u8>)> {
    if data.is_empty() {
        return vec![(0, Vec::new())];
    }
    data.chunks(chunk.max(1))
        .enumerate()
        .map(|(i, c)| ((i * chunk.max(1)) as u64, c.to_vec()))
        .collect()
}

impl FakeUpstream {
    /// Start serving.
    ///
    /// Binding failures do not panic: they are logged, and the returned upstream is down with an
    /// endpoint nobody listens on, so callers observe UNAVAILABLE.
    pub async fn start(opts: FakeUpstreamOpts) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                objects: BTreeMap::new(),
                changesets: BTreeMap::new(),
                refs: BTreeMap::new(),
                denied: BTreeSet::new(),
                redacted: BTreeMap::new(),
                corrupt: BTreeSet::new(),
                kill_after: None,
                down: false,
                policy_epoch: 0,
                redaction_epoch: 0,
                zstd: false,
                chunk_size: DEFAULT_CHUNK_SIZE,
                requests: Vec::new(),
            }),
            principal_of: opts.principal_of.clone(),
            public_keys: opts.public_keys.clone(),
        });
        match Self::serve(shared.clone(), opts.tls).await {
            Ok(up) => up,
            Err(e) => {
                tracing::error!(error = %e, "FakeUpstream failed to start");
                shared.state.lock().down = true;
                FakeUpstream {
                    shared,
                    endpoint: Endpoint::Unix("/nonexistent/scion-fake-upstream.sock".into()),
                    tls: None,
                    local_addr: None,
                    _dir: None,
                    task: None,
                }
            }
        }
    }

    async fn serve(shared: Arc<Shared>, tls: bool) -> Result<Self, Box<dyn std::error::Error>> {
        let svc = Svc(shared.clone());
        let mut builder = tonic::transport::Server::builder();
        let test_tls = if tls { Some(TestTls::generate()) } else { None };
        if let Some(t) = &test_tls {
            builder = builder.tls_config(tonic::transport::ServerTlsConfig::new().identity(
                tonic::transport::Identity::from_pem(&t.cert_pem, &t.key_pem),
            ))?;
        }
        let router = builder
            .layer(EpochTrailerLayer::new(shared.clone()))
            .add_service(v1::object_service_server::ObjectServiceServer::new(
                svc.clone(),
            ))
            .add_service(v1::query_service_server::QueryServiceServer::new(
                svc.clone(),
            ))
            .add_service(v1::auth_service_server::AuthServiceServer::new(svc.clone()))
            .add_service(v1::admin_service_server::AdminServiceServer::new(svc));
        if let Some(t) = test_tls {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;
            let task = tokio::spawn(async move {
                let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
                if let Err(e) = router.serve_with_incoming(incoming).await {
                    tracing::error!(error = %e, "FakeUpstream server stopped");
                }
            });
            Ok(FakeUpstream {
                shared,
                endpoint: Endpoint::Https(format!("https://localhost:{}", addr.port())),
                tls: Some(t.client_options()),
                local_addr: Some(addr),
                _dir: None,
                task: Some(task),
            })
        } else {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("upstream.sock");
            let listener = tokio::net::UnixListener::bind(&path)?;
            let task = tokio::spawn(async move {
                let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
                if let Err(e) = router.serve_with_incoming(incoming).await {
                    tracing::error!(error = %e, "FakeUpstream server stopped");
                }
            });
            Ok(FakeUpstream {
                shared,
                endpoint: Endpoint::Unix(path),
                tls: None,
                local_addr: None,
                _dir: Some(dir),
                task: Some(task),
            })
        }
    }

    /// The endpoint to connect to.
    pub fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }

    /// TLS client options when serving TLS.
    pub fn tls(&self) -> Option<TlsOptions> {
        self.tls.clone()
    }

    /// The TCP address when serving TLS (`None` for the Unix-socket mode).
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    /// Insert an object (`restricted` sets the access class).
    pub fn insert_object(&self, id: ObjectId, raw: Bytes, restricted: bool) {
        self.shared
            .state
            .lock()
            .objects
            .insert(id, StoredObject { raw, restricted });
    }

    /// Insert a changeset encoding.
    pub fn insert_changeset(&self, id: ObjectId, encoded: Bytes) {
        self.shared.state.lock().changesets.insert(id, encoded);
    }

    /// Set a ref.
    pub fn set_ref(&self, name: &str, target: ObjectId, version: u64) {
        self.shared
            .state
            .lock()
            .refs
            .entry(name.to_owned())
            .or_default()
            .insert(version, target);
    }

    /// Deny `id` to `principal`.
    pub fn deny(&self, principal: &str, id: ObjectId) {
        self.shared
            .state
            .lock()
            .denied
            .insert((principal.to_owned(), id));
    }

    /// Mark `id` redacted (and list it in ListRedactions). Each call bumps the redaction epoch
    /// and records the entry at the new epoch.
    pub fn redact(&self, id: ObjectId) {
        let mut s = self.shared.state.lock();
        s.redaction_epoch += 1;
        let e = s.redaction_epoch;
        s.redacted.insert(id, e);
    }

    /// Serve corrupted bytes for `id`.
    pub fn corrupt(&self, id: ObjectId) {
        self.shared.state.lock().corrupt.insert(id);
    }

    /// Kill GetObjects streams after `n` bytes (`None` = never).
    pub fn kill_stream_after_bytes(&self, n: Option<u64>) {
        self.shared.state.lock().kill_after = n;
    }

    /// When down, every RPC fails UNAVAILABLE.
    pub fn set_down(&self, down: bool) {
        self.shared.state.lock().down = down;
    }

    /// Epochs stamped on responses (tier server).
    pub fn set_epochs(&self, policy: u64, redaction: u64) {
        let mut s = self.shared.state.lock();
        s.policy_epoch = policy;
        s.redaction_epoch = redaction;
    }

    /// Serve full objects as zstd frames (`true`) or raw bytes (`false`, the default). Range
    /// reads are always raw (SPEC 7.3).
    pub fn set_zstd(&self, on: bool) {
        self.shared.state.lock().zstd = on;
    }

    /// Maximum data bytes per GetObjects chunk (default 64 KiB; minimum 1).
    pub fn set_chunk_size(&self, n: usize) {
        self.shared.state.lock().chunk_size = n.max(1);
    }

    /// Every recorded request.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.shared.state.lock().requests.clone()
    }

    /// Recorded requests for one method (full path, see [`methods`]).
    pub fn requests_for(&self, method: &str) -> Vec<RecordedRequest> {
        self.shared
            .state
            .lock()
            .requests
            .iter()
            .filter(|r| r.method == method)
            .cloned()
            .collect()
    }

    /// Forget recorded requests.
    pub fn reset_requests(&self) {
        self.shared.state.lock().requests.clear();
    }
}

#[derive(Clone)]
struct Svc(Arc<Shared>);

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

fn ids_lossy(v: &[Vec<u8>]) -> Vec<ObjectId> {
    v.iter()
        .filter_map(|b| ObjectId::from_bytes(b).ok())
        .collect()
}

fn parse_ids(v: &[Vec<u8>]) -> Result<Vec<ObjectId>, Status> {
    scion_proto::convert::parse_ids(v, "ids").map_err(|e| status_from_error(&e))
}

fn unimplemented(method: &str) -> Status {
    Status::unimplemented(format!("{method} is not implemented by FakeUpstream"))
}

impl Svc {
    fn principal<T>(&self, req: &Request<T>) -> Option<String> {
        let v = req.metadata().get("authorization")?.to_str().ok()?;
        let token = v.strip_prefix("Bearer ")?;
        (self.0.principal_of)(token)
    }

    /// Record the request, then apply the down knob and (optionally) authentication.
    fn begin<T>(
        &self,
        method: &str,
        req: &Request<T>,
        ids: Vec<ObjectId>,
        require_auth: bool,
    ) -> Result<Option<String>, Status> {
        let principal = self.principal(req);
        let mut s = self.0.state.lock();
        s.requests.push(RecordedRequest {
            method: method.to_owned(),
            ids,
            principal: principal.clone(),
        });
        if s.down {
            return Err(status_from_error(&ScionError::unavailable(
                subcode::STORE_UNAVAILABLE,
                "fake upstream is down",
            )));
        }
        if require_auth && principal.is_none() {
            return Err(status_from_error(&ScionError::unauthenticated(
                "missing or invalid bearer token",
            )));
        }
        Ok(principal)
    }
}

/// Build the GetObjects response items; the stream applies the kill knob afterwards.
fn object_chunks(
    s: &State,
    principal: &str,
    ids: &[ObjectId],
    offset: Option<u64>,
    length: Option<u64>,
) -> Vec<v1::ObjectChunk> {
    let mut out = Vec::new();
    for id in ids {
        let status_only = |st: v1::ItemStatus| v1::ObjectChunk {
            id: id.as_bytes().to_vec(),
            status: st as i32,
            r#final: true,
            ..Default::default()
        };
        if s.redacted.contains_key(id) {
            out.push(status_only(v1::ItemStatus::Redacted));
            continue;
        }
        let Some(obj) = s.objects.get(id) else {
            out.push(status_only(v1::ItemStatus::NotFound));
            continue;
        };
        if s.denied.contains(&(principal.to_owned(), *id)) {
            out.push(status_only(v1::ItemStatus::Denied));
            continue;
        }
        let raw = if s.corrupt.contains(id) {
            corrupted(&obj.raw)
        } else {
            obj.raw.clone()
        };
        let total = raw.len() as u64;
        let access = if obj.restricted {
            v1::AccessClass::Restricted
        } else {
            v1::AccessClass::Root
        };
        let ranged = offset.is_some() || length.is_some();
        let (encoding, base, data): (v1::Encoding, u64, Vec<u8>) = if ranged {
            let start = offset.unwrap_or(0).min(total);
            let end = start.saturating_add(length.unwrap_or(u64::MAX)).min(total);
            (
                v1::Encoding::Raw,
                start,
                raw[start as usize..end as usize].to_vec(),
            )
        } else if s.zstd {
            match zstd::bulk::compress(&raw, ZSTD_LEVEL) {
                Ok(frame) => (v1::Encoding::Zstd, 0, frame),
                Err(e) => {
                    tracing::warn!(error = %e, "zstd failed; serving raw");
                    (v1::Encoding::Raw, 0, raw.to_vec())
                }
            }
        } else {
            (v1::Encoding::Raw, 0, raw.to_vec())
        };
        let parts = split_chunks(&data, s.chunk_size);
        let n = parts.len();
        for (i, (off, chunk)) in parts.into_iter().enumerate() {
            out.push(v1::ObjectChunk {
                id: id.as_bytes().to_vec(),
                status: v1::ItemStatus::Ok as i32,
                encoding: encoding as i32,
                data: chunk,
                total_size: total,
                access_class: access as i32,
                r#final: i + 1 == n,
                offset: base + off,
            });
        }
    }
    out
}

/// Apply `kill_after`: emit at most `n` data bytes, then fail the stream with UNAVAILABLE.
fn apply_kill(
    chunks: Vec<v1::ObjectChunk>,
    kill_after: Option<u64>,
) -> Vec<Result<v1::ObjectChunk, Status>> {
    let Some(limit) = kill_after else {
        return chunks.into_iter().map(Ok).collect();
    };
    let mut out = Vec::new();
    let mut sent: u64 = 0;
    for mut c in chunks {
        let len = c.data.len() as u64;
        if sent + len <= limit {
            sent += len;
            out.push(Ok(c));
            continue;
        }
        let room = (limit - sent) as usize;
        if room > 0 {
            c.data.truncate(room);
            c.r#final = false;
            out.push(Ok(c));
        }
        out.push(Err(status_from_error(&ScionError::unavailable(
            subcode::INJECTED,
            format!("stream killed after {limit} bytes"),
        ))));
        return out;
    }
    out
}

#[tonic::async_trait]
impl v1::object_service_server::ObjectService for Svc {
    async fn get_server_info(
        &self,
        req: Request<v1::GetServerInfoRequest>,
    ) -> Result<Response<v1::ServerInfo>, Status> {
        self.begin(methods::GET_SERVER_INFO, &req, Vec::new(), false)?;
        let s = self.0.state.lock();
        Ok(Response::new(v1::ServerInfo {
            protocol_versions: vec!["scion.v1".to_owned()],
            repo: "fake".to_owned(),
            policy_epoch: s.policy_epoch,
            redaction_epoch: s.redaction_epoch,
            token_public_keys: self
                .0
                .public_keys
                .iter()
                .map(|(kid, k)| scion_proto::convert::public_key_to_proto(kid, k))
                .collect(),
            limits: Some(v1::LimitsMsg::from(&scion_core::Limits::default())),
            github_client_id: String::new(),
            github_oauth_base_url: String::new(),
        }))
    }

    type GetObjectsStream = BoxStream<v1::ObjectChunk>;

    async fn get_objects(
        &self,
        req: Request<v1::GetObjectsRequest>,
    ) -> Result<Response<Self::GetObjectsStream>, Status> {
        let principal = self
            .begin(
                methods::GET_OBJECTS,
                &req,
                ids_lossy(&req.get_ref().ids),
                true,
            )?
            .unwrap_or_default();
        let r = req.into_inner();
        let ids = parse_ids(&r.ids)?;
        let s = self.0.state.lock();
        let chunks = object_chunks(&s, &principal, &ids, r.offset, r.length);
        let items = apply_kill(chunks, s.kill_after);
        drop(s);
        Ok(Response::new(Box::pin(tokio_stream::iter(items))))
    }

    async fn has_objects(
        &self,
        req: Request<v1::HasObjectsRequest>,
    ) -> Result<Response<v1::HasObjectsResponse>, Status> {
        let principal = self
            .begin(
                methods::HAS_OBJECTS,
                &req,
                ids_lossy(&req.get_ref().ids),
                true,
            )?
            .unwrap_or_default();
        let ids = parse_ids(&req.get_ref().ids)?;
        let s = self.0.state.lock();
        let present = ids
            .iter()
            .map(|id| {
                s.objects.contains_key(id)
                    && !s.redacted.contains_key(id)
                    && !s.denied.contains(&(principal.clone(), *id))
            })
            .collect();
        Ok(Response::new(v1::HasObjectsResponse { present }))
    }

    async fn resolve_ref(
        &self,
        req: Request<v1::ResolveRefRequest>,
    ) -> Result<Response<v1::ResolveRefResponse>, Status> {
        self.begin(methods::RESOLVE_REF, &req, Vec::new(), true)?;
        let r = req.into_inner();
        let s = self.0.state.lock();
        let found = s.refs.get(&r.name).and_then(|versions| match r.at_version {
            Some(v) => versions.get(&v).map(|t| (v, *t)),
            None => versions.iter().next_back().map(|(v, t)| (*v, *t)),
        });
        match found {
            Some((version, target)) => Ok(Response::new(v1::ResolveRefResponse {
                target: target.as_bytes().to_vec(),
                version,
            })),
            None => Err(status_from_error(&ScionError::not_found(format!(
                "ref {} not found",
                r.name
            )))),
        }
    }
}

fn resolve_revision(s: &State, rev: &str) -> Result<ObjectId, ScionError> {
    if let Some(t) = s.refs.get(rev).and_then(|v| v.values().next_back()) {
        return Ok(*t);
    }
    if rev.contains(['~', '^', '@']) {
        return Err(ScionError::invalid(
            subcode::INVALID_REVISION,
            "FakeUpstream does not support revision suffixes",
        ));
    }
    if let Ok(id) = ObjectId::parse_text(rev)
        && s.changesets.contains_key(&id)
    {
        return Ok(id);
    }
    let hex = rev
        .strip_prefix("c1")
        .filter(|h| h.len() >= 8)
        .unwrap_or(rev);
    if hex.len() >= 8
        && hex.len() <= 64
        && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        let matches: Vec<ObjectId> = s
            .changesets
            .keys()
            .filter(|id| id.hex().starts_with(hex))
            .copied()
            .collect();
        match matches.len() {
            0 => {}
            1 => return Ok(matches[0]),
            _ => {
                return Err(ScionError::new(
                    scion_core::ErrorCode::InvalidArgument,
                    format!("ambiguous prefix {hex}"),
                )
                .with_payload(ErrorPayload::Candidates(matches)));
            }
        }
    }
    Err(ScionError::not_found(format!("revision {rev} not found")))
}

#[tonic::async_trait]
impl v1::query_service_server::QueryService for Svc {
    async fn get_changesets(
        &self,
        req: Request<v1::GetChangesetsRequest>,
    ) -> Result<Response<v1::GetChangesetsResponse>, Status> {
        let principal = self
            .begin(
                methods::GET_CHANGESETS,
                &req,
                ids_lossy(&req.get_ref().ids),
                true,
            )?
            .unwrap_or_default();
        let ids = parse_ids(&req.get_ref().ids)?;
        let s = self.0.state.lock();
        let items = ids
            .iter()
            .map(|id| {
                let mut item = v1::ChangesetItem {
                    id: id.as_bytes().to_vec(),
                    ..Default::default()
                };
                match s.changesets.get(id) {
                    None => item.status = v1::ItemStatus::NotFound as i32,
                    Some(_) if s.denied.contains(&(principal.clone(), *id)) => {
                        item.status = v1::ItemStatus::Denied as i32
                    }
                    Some(enc) => {
                        item.status = v1::ItemStatus::Ok as i32;
                        item.view = Some(v1::changeset_item::View::Encoded(enc.to_vec()));
                    }
                }
                item
            })
            .collect();
        Ok(Response::new(v1::GetChangesetsResponse { items }))
    }

    type ReadPathStream = BoxStream<v1::ReadPathChunk>;

    async fn read_path(
        &self,
        req: Request<v1::ReadPathRequest>,
    ) -> Result<Response<Self::ReadPathStream>, Status> {
        let m = "/scion.v1.QueryService/ReadPath";
        self.begin(m, &req, Vec::new(), false)?;
        Err(unimplemented(m))
    }

    async fn list_tree(
        &self,
        req: Request<v1::ListTreeRequest>,
    ) -> Result<Response<v1::ListTreeResponse>, Status> {
        let m = "/scion.v1.QueryService/ListTree";
        self.begin(m, &req, Vec::new(), false)?;
        Err(unimplemented(m))
    }

    async fn log(&self, req: Request<v1::LogRequest>) -> Result<Response<v1::LogResponse>, Status> {
        let m = "/scion.v1.QueryService/Log";
        self.begin(m, &req, Vec::new(), false)?;
        Err(unimplemented(m))
    }

    async fn blame(
        &self,
        req: Request<v1::BlameRequest>,
    ) -> Result<Response<v1::BlameResponse>, Status> {
        let m = "/scion.v1.QueryService/Blame";
        self.begin(m, &req, Vec::new(), false)?;
        Err(unimplemented(m))
    }

    type DiffStream = BoxStream<v1::DiffItem>;

    async fn diff(
        &self,
        req: Request<v1::DiffRequest>,
    ) -> Result<Response<Self::DiffStream>, Status> {
        let m = "/scion.v1.QueryService/Diff";
        self.begin(m, &req, Vec::new(), false)?;
        Err(unimplemented(m))
    }

    async fn is_ancestor(
        &self,
        req: Request<v1::IsAncestorRequest>,
    ) -> Result<Response<v1::IsAncestorResponse>, Status> {
        let m = "/scion.v1.QueryService/IsAncestor";
        self.begin(m, &req, Vec::new(), false)?;
        Err(unimplemented(m))
    }

    async fn merge_base(
        &self,
        req: Request<v1::MergeBaseRequest>,
    ) -> Result<Response<v1::MergeBaseResponse>, Status> {
        let m = "/scion.v1.QueryService/MergeBase";
        self.begin(m, &req, Vec::new(), false)?;
        Err(unimplemented(m))
    }

    async fn resolve(
        &self,
        req: Request<v1::ResolveRequest>,
    ) -> Result<Response<v1::ResolveResponse>, Status> {
        self.begin(methods::RESOLVE, &req, Vec::new(), true)?;
        let s = self.0.state.lock();
        match resolve_revision(&s, &req.get_ref().revision) {
            Ok(id) => Ok(Response::new(v1::ResolveResponse {
                changeset: id.as_bytes().to_vec(),
            })),
            Err(e) => Err(status_from_error(&e)),
        }
    }

    async fn get_derived(
        &self,
        req: Request<v1::GetDerivedRequest>,
    ) -> Result<Response<v1::GetDerivedResponse>, Status> {
        let m = "/scion.v1.QueryService/GetDerived";
        self.begin(m, &req, Vec::new(), false)?;
        Err(unimplemented(m))
    }
}

#[tonic::async_trait]
impl v1::auth_service_server::AuthService for Svc {
    async fn login(
        &self,
        req: Request<v1::LoginRequest>,
    ) -> Result<Response<v1::LoginResponse>, Status> {
        let m = "/scion.v1.AuthService/Login";
        self.begin(m, &req, Vec::new(), false)?;
        Err(unimplemented(m))
    }

    async fn who_am_i(
        &self,
        req: Request<v1::WhoAmIRequest>,
    ) -> Result<Response<v1::WhoAmIResponse>, Status> {
        let principal = self
            .begin(methods::WHO_AM_I, &req, Vec::new(), true)?
            .unwrap_or_default();
        Ok(Response::new(v1::WhoAmIResponse {
            principal,
            groups: Vec::new(),
            expires_at_unix: 0,
        }))
    }

    async fn check_access(
        &self,
        req: Request<v1::CheckAccessRequest>,
    ) -> Result<Response<v1::CheckAccessResponse>, Status> {
        self.begin(
            methods::CHECK_ACCESS,
            &req,
            ids_lossy(&req.get_ref().object_ids),
            true,
        )?;
        let r = req.into_inner();
        let Some(subject) = (self.0.principal_of)(&r.subject_token) else {
            return Err(status_from_error(&ScionError::unauthenticated(
                "invalid subject token",
            )));
        };
        let ids = scion_proto::convert::parse_ids(&r.object_ids, "object_ids")
            .map_err(|e| status_from_error(&e))?;
        let s = self.0.state.lock();
        let mut allowed = Vec::with_capacity(ids.len());
        let mut access_class = Vec::with_capacity(ids.len());
        for id in &ids {
            let visible = s.objects.get(id).filter(|_| !s.redacted.contains_key(id));
            let ok = visible.is_some() && !s.denied.contains(&(subject.clone(), *id));
            allowed.push(ok);
            access_class.push(match visible {
                Some(o) if o.restricted => v1::AccessClass::Restricted as i32,
                Some(_) => v1::AccessClass::Root as i32,
                None => v1::AccessClass::Unspecified as i32,
            });
        }
        Ok(Response::new(v1::CheckAccessResponse {
            allowed,
            access_class,
        }))
    }
}

#[tonic::async_trait]
impl v1::admin_service_server::AdminService for Svc {
    async fn redact(
        &self,
        req: Request<v1::RedactRequest>,
    ) -> Result<Response<v1::RedactResponse>, Status> {
        let m = "/scion.v1.AdminService/Redact";
        self.begin(m, &req, ids_lossy(&req.get_ref().object_ids), false)?;
        Err(unimplemented(m))
    }

    async fn unredact(
        &self,
        req: Request<v1::UnredactRequest>,
    ) -> Result<Response<v1::UnredactResponse>, Status> {
        let m = "/scion.v1.AdminService/Unredact";
        self.begin(m, &req, ids_lossy(&req.get_ref().object_ids), false)?;
        Err(unimplemented(m))
    }

    async fn purge_now(
        &self,
        req: Request<v1::PurgeNowRequest>,
    ) -> Result<Response<v1::PurgeNowResponse>, Status> {
        let m = "/scion.v1.AdminService/PurgeNow";
        self.begin(m, &req, ids_lossy(&req.get_ref().object_ids), false)?;
        Err(unimplemented(m))
    }

    async fn list_redactions(
        &self,
        req: Request<v1::ListRedactionsRequest>,
    ) -> Result<Response<v1::ListRedactionsResponse>, Status> {
        self.begin(methods::LIST_REDACTIONS, &req, Vec::new(), true)?;
        let since = req.get_ref().since_epoch;
        let s = self.0.state.lock();
        let mut entries: Vec<(u64, ObjectId)> = s
            .redacted
            .iter()
            .filter(|(_, e)| **e > since)
            .map(|(id, e)| (*e, *id))
            .collect();
        entries.sort();
        Ok(Response::new(v1::ListRedactionsResponse {
            entries: entries
                .into_iter()
                .map(|(epoch, id)| v1::RedactionEntry {
                    object_id: id.as_bytes().to_vec(),
                    state: "quarantined".to_owned(),
                    epoch,
                    reason: "fake".to_owned(),
                })
                .collect(),
            current_epoch: s.redaction_epoch,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(len: usize) -> v1::ObjectChunk {
        v1::ObjectChunk {
            data: vec![1; len],
            r#final: true,
            ..Default::default()
        }
    }

    #[test]
    fn kill_truncates_exactly() {
        let out = apply_kill(vec![chunk(10), chunk(10), chunk(10)], Some(15));
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].as_ref().unwrap().data.len(), 10);
        let c = out[1].as_ref().unwrap();
        assert_eq!(c.data.len(), 5);
        assert!(!c.r#final);
        assert!(out[2].is_err());

        let out = apply_kill(vec![chunk(10), chunk(10)], Some(10));
        assert_eq!(out.len(), 2);
        assert!(out[1].is_err());

        let out = apply_kill(vec![chunk(10)], Some(0));
        assert_eq!(out.len(), 1);
        assert!(out[0].is_err());

        // Status-only items carry no data and never trip the limit.
        let out = apply_kill(vec![chunk(0), chunk(0)], Some(0));
        assert!(out.iter().all(|r| r.is_ok()));

        let out = apply_kill(vec![chunk(10)], None);
        assert!(out[0].is_ok());
    }

    #[test]
    fn splitting() {
        assert_eq!(split_chunks(&[], 4), vec![(0, vec![])]);
        let p = split_chunks(&[1, 2, 3, 4, 5], 2);
        assert_eq!(p, vec![(0, vec![1, 2]), (2, vec![3, 4]), (4, vec![5])]);
    }

    #[test]
    fn corruption_changes_bytes() {
        assert_ne!(
            corrupted(&Bytes::from_static(b"abc")),
            Bytes::from_static(b"abc")
        );
        assert_eq!(corrupted(&Bytes::new()).len(), 1);
    }
}
