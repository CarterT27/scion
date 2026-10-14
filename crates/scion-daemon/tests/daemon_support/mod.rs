//! Shared helpers for the scion-daemon integration tests: a FakeUpstream whose tokens are
//! verified by a test Keyring, a daemon in front of it (optionally through a gate that holds
//! GetObjects calls), clients and response normalization.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use futures::future::BoxFuture;
use scion_auth::Keyring;
use scion_config::DaemonConfig;
use scion_core::{Clock, Kind, ManualClock, ObjectId, Principal, SystemClock};
use scion_daemon::{Daemon, DaemonHandle, DirectUpstream, UpstreamConnector};
use scion_obs::Metrics;
use scion_proto::{Endpoint, ScionClient, v1};
use scion_testkit::{FakeUpstream, FakeUpstreamOpts};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};
use tonic::body::Body;
use tower::ServiceExt;

pub const ALICE: &str = "github:alice";
pub const BOB: &str = "github:bob";

/// Options of a [`Harness`].
pub struct Opts {
    pub manual_clock: bool,
    pub gate: bool,
    /// Serve the FakeUpstream over TLS (SPEC 17: TLS between daemon and server).
    pub tls: bool,
    pub tweak: Box<dyn FnOnce(&mut DaemonConfig) + Send>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            manual_clock: false,
            gate: false,
            tls: false,
            tweak: Box::new(|_| {}),
        }
    }
}

/// Holds GetObjects calls between the daemon and the upstream until opened.
pub struct Gate {
    open: tokio::sync::watch::Sender<bool>,
    waiting: AtomicUsize,
    passed: AtomicUsize,
    alt: parking_lot::Mutex<Option<tonic::transport::Channel>>,
}

impl Gate {
    /// Route every later call to `alt` (another upstream replica) instead of the upstream.
    pub fn route_to(&self, alt: Option<tonic::transport::Channel>) {
        *self.alt.lock() = alt;
    }
    pub fn set_open(&self, on: bool) {
        self.open.send_replace(on);
    }
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }
    pub fn passed(&self) -> usize {
        self.passed.load(Ordering::SeqCst)
    }
}

#[derive(Clone)]
struct GateSvc {
    gate: Arc<Gate>,
    ch: tonic::transport::Channel,
}

impl tower_service::Service<http::Request<Body>> for GateSvc {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        let gate = self.gate.clone();
        let ch = gate.alt.lock().clone().unwrap_or_else(|| self.ch.clone());
        Box::pin(async move {
            if req.uri().path() == scion_testkit::upstream_methods::GET_OBJECTS {
                gate.waiting.fetch_add(1, Ordering::SeqCst);
                let mut rx = gate.open.subscribe();
                let _ = rx.wait_for(|o| *o).await;
                gate.waiting.fetch_sub(1, Ordering::SeqCst);
                gate.passed.fetch_add(1, Ordering::SeqCst);
            }
            match ch.oneshot(req).await {
                Ok(r) => Ok(r),
                Err(e) => Ok(tonic::Status::unavailable(e.to_string()).into_http()),
            }
        })
    }
}

/// A test environment: keyring, FakeUpstream, optional gate, daemon.
pub struct Harness {
    pub keyring: Keyring,
    pub up: FakeUpstream,
    pub gate: Option<Arc<Gate>>,
    pub conn: Arc<DirectUpstream>,
    pub upstream_endpoint: Endpoint,
    pub daemon: Option<DaemonHandle>,
    pub clock: Option<Arc<ManualClock>>,
    pub metrics: Arc<Metrics>,
    pub cfg: DaemonConfig,
    pub dir: tempfile::TempDir,
    _gate_dir: Option<tempfile::TempDir>,
    _gate_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(t) = &self._gate_task {
            t.abort();
        }
    }
}

pub fn upstream_opts(keyring: &Keyring) -> FakeUpstreamOpts {
    upstream_opts_tls(keyring, false)
}

pub fn upstream_opts_tls(keyring: &Keyring, tls: bool) -> FakeUpstreamOpts {
    let verifier = keyring.verifier();
    FakeUpstreamOpts {
        tls,
        principal_of: Arc::new(move |t: &str| {
            verifier
                .verify(t, SystemTime::now())
                .ok()
                .map(|c| c.sub.id_string())
        }),
        public_keys: keyring
            .public_keys()
            .into_iter()
            .map(|k| (k.kid, k.ed25519_public))
            .collect(),
    }
}

impl Harness {
    pub async fn start(opts: Opts) -> Harness {
        let keyring = Keyring::generate_for_tests(1);
        let up = FakeUpstream::start(upstream_opts_tls(&keyring, opts.tls)).await;
        let (gate, gate_dir, gate_task, upstream_endpoint) = if opts.gate {
            let ch = ScionClient::connect(&up.endpoint(), up.tls(), None)
                .await
                .unwrap()
                .channel();
            let gate = Arc::new(Gate {
                open: tokio::sync::watch::channel(true).0,
                waiting: AtomicUsize::new(0),
                passed: AtomicUsize::new(0),
                alt: parking_lot::Mutex::new(None),
            });
            let d = tempfile::tempdir().unwrap();
            let sock = d.path().join("gate.sock");
            let listener = tokio::net::UnixListener::bind(&sock).unwrap();
            let svc = GateSvc {
                gate: gate.clone(),
                ch,
            };
            let task = tokio::spawn(async move {
                let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
                let _ = tonic::transport::Server::builder()
                    .serve_with_incoming(svc, incoming)
                    .await;
            });
            (Some(gate), Some(d), Some(task), Endpoint::Unix(sock))
        } else {
            (None, None, None, up.endpoint())
        };
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = DaemonConfig::for_tests(dir.path(), &upstream_endpoint.to_string());
        (opts.tweak)(&mut cfg);
        let clock = opts
            .manual_clock
            .then(|| ManualClock::new(SystemTime::now()));
        let clock_dyn: Arc<dyn Clock> = match &clock {
            Some(c) => c.clone(),
            None => Arc::new(SystemClock),
        };
        let conn = Arc::new(DirectUpstream::new(upstream_endpoint.clone(), up.tls()));
        let metrics = Metrics::new();
        let daemon = Daemon::start(cfg.clone(), conn.clone(), clock_dyn, metrics.clone())
            .await
            .unwrap();
        Harness {
            keyring,
            up,
            gate,
            conn,
            upstream_endpoint,
            daemon: Some(daemon),
            clock,
            metrics,
            cfg,
            dir,
            _gate_dir: gate_dir,
            _gate_task: gate_task,
        }
    }

    pub fn daemon(&self) -> &DaemonHandle {
        self.daemon.as_ref().unwrap()
    }

    pub fn now(&self) -> SystemTime {
        match &self.clock {
            Some(c) => c.now(),
            None => SystemTime::now(),
        }
    }

    pub fn token(&self, who: &str) -> String {
        self.keyring
            .issue(
                &Principal::parse(who).unwrap(),
                Duration::from_secs(3600),
                self.now(),
            )
            .unwrap()
            .0
    }

    pub async fn via_daemon(&self, who: &str) -> ScionClient {
        ScionClient::connect(&self.daemon().endpoint(), None, Some(self.token(who)))
            .await
            .unwrap()
    }

    pub async fn direct(&self, who: &str) -> ScionClient {
        ScionClient::connect(&self.up.endpoint(), self.up.tls(), Some(self.token(who)))
            .await
            .unwrap()
    }

    /// Stop the daemon (cache survives).
    pub async fn stop_daemon(&mut self) {
        if let Some(d) = self.daemon.take() {
            d.shutdown().await;
        }
    }

    /// Start a new daemon over the same config and cache.
    pub async fn restart_daemon(&mut self) {
        self.stop_daemon().await;
        let clock_dyn: Arc<dyn Clock> = match &self.clock {
            Some(c) => c.clone(),
            None => Arc::new(SystemClock),
        };
        let conn: Arc<dyn UpstreamConnector> = self.conn.clone();
        self.daemon = Some(
            Daemon::start(self.cfg.clone(), conn, clock_dyn, self.metrics.clone())
                .await
                .unwrap(),
        );
    }

    /// Upstream GetObjects calls recorded so far.
    pub fn upstream_get_objects(&self) -> usize {
        self.up
            .requests_for(scion_testkit::upstream_methods::GET_OBJECTS)
            .len()
    }

    pub fn upstream_calls(&self, method: &str) -> usize {
        self.up.requests_for(method).len()
    }

    /// Insert a blob with deterministic content of `len` bytes into the upstream.
    pub fn blob(&self, seed: u64, len: usize, restricted: bool) -> (ObjectId, Bytes) {
        let data = blob_bytes(seed, len);
        let id = ObjectId::hash_blob(&data);
        self.up.insert_object(id, data.clone(), restricted);
        (id, data)
    }

    /// The cache file of `id` (SPEC 10.2 layout).
    pub fn cache_file(&self, id: &ObjectId) -> std::path::PathBuf {
        let hex = id.hex();
        self.cfg
            .cache_dir
            .join("objects")
            .join(id.kind().letter().to_string())
            .join(&hex[..2])
            .join(hex)
    }
}

pub fn blob_bytes(seed: u64, len: usize) -> Bytes {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut v = Vec::with_capacity(len);
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.push((x % 251) as u8);
    }
    Bytes::from(v)
}

/// A synthetic id of `kind` that no upstream holds.
pub fn missing_id(seed: u8) -> ObjectId {
    ObjectId::new(Kind::Blob, [seed; 32])
}

/// One item of a GetObjects answer, normalized: chunks joined and zstd decompressed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: ObjectId,
    pub status: i32,
    pub access_class: i32,
    pub total_size: u64,
    pub content: Vec<u8>,
    pub first_offset: u64,
}

pub async fn get_objects_raw(
    c: &ScionClient,
    ids: &[ObjectId],
    offset: Option<u64>,
    length: Option<u64>,
) -> Result<Vec<v1::ObjectChunk>, tonic::Status> {
    let mut s = c
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: ids.iter().map(|i| i.as_bytes().to_vec()).collect(),
            offset,
            length,
        })
        .await?
        .into_inner();
    let mut out = Vec::new();
    while let Some(m) = s.message().await? {
        out.push(m);
    }
    Ok(out)
}

pub fn normalize(chunks: Vec<v1::ObjectChunk>) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    let mut frames: Vec<(i32, Vec<u8>)> = Vec::new();
    let mut open = false;
    for c in chunks {
        let id = ObjectId::from_bytes(&c.id).unwrap();
        if !open {
            out.push(Item {
                id,
                status: c.status,
                access_class: c.access_class,
                total_size: c.total_size,
                content: Vec::new(),
                first_offset: c.offset,
            });
            frames.push((c.encoding, Vec::new()));
        }
        let last = frames.last_mut().unwrap();
        assert_eq!(out.last().unwrap().id, id, "chunks of items interleaved");
        last.1.extend_from_slice(&c.data);
        open = !c.r#final;
    }
    assert!(!open, "stream ended inside an item");
    for (item, (enc, data)) in out.iter_mut().zip(frames) {
        item.content = if enc == v1::Encoding::Zstd as i32 {
            zstd::stream::decode_all(&data[..]).unwrap()
        } else {
            data
        };
    }
    out
}

pub async fn get_objects(c: &ScionClient, ids: &[ObjectId]) -> Result<Vec<Item>, tonic::Status> {
    Ok(normalize(get_objects_raw(c, ids, None, None).await?))
}

pub async fn get_one(c: &ScionClient, id: ObjectId) -> Result<Item, tonic::Status> {
    let mut v = get_objects(c, &[id]).await?;
    assert_eq!(v.len(), 1);
    Ok(v.remove(0))
}

pub const OK: i32 = v1::ItemStatus::Ok as i32;
pub const NOT_FOUND: i32 = v1::ItemStatus::NotFound as i32;
pub const DENIED: i32 = v1::ItemStatus::Denied as i32;
pub const REDACTED: i32 = v1::ItemStatus::Redacted as i32;

/// Wait until `cond` holds, polling every 10ms, for at most `max`.
pub async fn eventually(max: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + max;
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Map of a metrics text exposition: `name{labels}` -> value.
pub fn metric_lines(text: &str) -> BTreeMap<String, f64> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            let (k, v) = l.rsplit_once(' ')?;
            Some((k.to_owned(), v.parse().ok()?))
        })
        .collect()
}

/// Sum of every series of metric `name` whose labels contain `needle`.
pub fn metric_sum(text: &str, name: &str, needle: &str) -> f64 {
    metric_lines(text)
        .into_iter()
        .filter(|(k, _)| (k == name || k.starts_with(&format!("{name}{{"))) && k.contains(needle))
        .map(|(_, v)| v)
        .sum()
}
