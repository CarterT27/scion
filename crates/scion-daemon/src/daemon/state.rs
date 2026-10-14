//! Shared daemon state: caches, upstream connection, epochs, token verification (SPEC 10,
//! 12.1, 13.3; PLAN 6.13).

use crate::upstream::UpstreamConnector;
use parking_lot::Mutex;
use scion_auth::{PublicKeyInfo, Verifier};
use scion_cache::{AccessCache, Coalescer, DiskCache, LocalRedactions, RefCache, UpstreamLimiter};
use scion_config::DaemonConfig;
use scion_core::{Clock, ErrorCode, Kind, ObjectId, Result, ScionError, Tier, subcode};
use scion_obs::{Metrics, RpcLogEvent};
use scion_proto::{EpochSource, Epochs, ScionClient, error_from_status, read_epochs, v1};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};
use tokio_util::sync::CancellationToken;

use super::fetch::{Batch, FetchItem};

/// The tier label of every daemon metric and log event (SPEC 15).
pub(crate) const TIER: &str = "daemon";
/// SPEC 10.5: never more than three upstream attempts per request.
pub(crate) const MAX_ATTEMPTS: u32 = 3;
/// First retry delay; doubled per attempt, with jitter.
const BACKOFF_BASE: Duration = Duration::from_millis(25);
/// Minimum wall time between key refreshes triggered by unknown `kid`s.
const KEY_REFRESH_MIN: Duration = Duration::from_secs(1);
/// Minimum wall time between synchronous initial redaction syncs attempted by requests.
const SYNC_RETRY_MIN: Duration = Duration::from_millis(500);
/// Bound on remembered access classes (cleared wholesale when exceeded).
const MAX_CLASSES: usize = 1 << 20;

/// An authenticated caller (SPEC 12.1): the bearer token and the verified principal id.
#[derive(Clone)]
pub(crate) struct Caller {
    pub token: String,
    pub principal: String,
}

impl std::fmt::Debug for Caller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the token (PLAN 6.9).
        f.debug_struct("Caller")
            .field("principal", &self.principal)
            .finish_non_exhaustive()
    }
}

/// Everything the daemon's tasks and handlers share.
pub(crate) struct Inner {
    pub cfg: DaemonConfig,
    pub upstream: Arc<dyn UpstreamConnector>,
    client: tokio::sync::Mutex<Option<ScionClient>>,
    pub clock: Arc<dyn Clock>,
    base_instant: Instant,
    base_time: SystemTime,
    pub metrics: Arc<Metrics>,
    pub cache: Arc<DiskCache>,
    pub coalescer: Coalescer<ObjectId, FetchItem>,
    /// Missing ids with an upstream batch fetch in flight (SPEC 10.2 coalescing).
    pub inflight: Mutex<HashMap<ObjectId, Arc<Batch>>>,
    pub limiter: UpstreamLimiter,
    pub refs: RefCache,
    pub redactions: LocalRedactions,
    pub access: AccessCache,
    /// Access classes learned from upstream (dropped on a policy epoch change, SPEC 10.4).
    classes: Mutex<HashMap<ObjectId, i32>>,
    /// Principals with a successful upstream ResolveRef within the access TTL.
    ref_readers: Mutex<HashMap<String, Instant>>,
    verifier: parking_lot::RwLock<Option<Verifier>>,
    keys_refreshed_at: Mutex<Option<Instant>>,
    server_info: Mutex<Option<(v1::ServerInfo, Instant)>>,
    policy_epoch: Mutex<Option<u64>>,
    upstream_redaction_epoch: AtomicU64,
    /// A verified caller token (and its expiry) used for ListRedactions polls (PLAN 6.12).
    poll_token: Mutex<Option<(String, SystemTime)>>,
    pub poll_lock: tokio::sync::Mutex<()>,
    pub synced: AtomicBool,
    last_sync_attempt: Mutex<Option<Instant>>,
    pub poll_blocked: AtomicBool,
    pub poll_now: tokio::sync::Notify,
    pub upstream_requests: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    integrity_seen: AtomicU64,
    jitter: AtomicU64,
    pub shutdown: CancellationToken,
}

/// Lowercase kind name for the `kind` metric label (SPEC 15.2).
pub(crate) fn kind_label(k: Kind) -> &'static str {
    match k {
        Kind::Blob => "blob",
        Kind::Tree => "tree",
        Kind::Changeset => "changeset",
        Kind::Meta => "meta",
        Kind::Derived(_) => "derived",
    }
}

/// The bearer token of a request, if any (SPEC 7.1).
pub(crate) fn bearer(h: &http::HeaderMap) -> Option<String> {
    let v = h.get("authorization")?.to_str().ok()?;
    let t = v.strip_prefix("Bearer ")?.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// splitmix64: cheap deterministic jitter without OS randomness (PLAN 5.1).
fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Inner {
    pub fn new(
        cfg: DaemonConfig,
        upstream: Arc<dyn UpstreamConnector>,
        clock: Arc<dyn Clock>,
        metrics: Arc<Metrics>,
        cache: Arc<DiskCache>,
    ) -> Self {
        let base_instant = Instant::now();
        let base_time = clock.now();
        let limiter = UpstreamLimiter::new(cfg.upstream_max_concurrency, cfg.upstream_queue);
        let refs = RefCache::new(Duration::from_millis(cfg.ref_cache_ttl_ms));
        let access = AccessCache::new(Duration::from_millis(cfg.access_cache_ttl_ms));
        Inner {
            upstream,
            client: tokio::sync::Mutex::new(None),
            clock,
            base_instant,
            base_time,
            metrics,
            cache,
            coalescer: Coalescer::new(),
            inflight: Mutex::new(HashMap::new()),
            limiter,
            refs,
            redactions: LocalRedactions::new(base_instant),
            access,
            classes: Mutex::new(HashMap::new()),
            ref_readers: Mutex::new(HashMap::new()),
            verifier: parking_lot::RwLock::new(None),
            keys_refreshed_at: Mutex::new(None),
            server_info: Mutex::new(None),
            policy_epoch: Mutex::new(None),
            upstream_redaction_epoch: AtomicU64::new(0),
            poll_token: Mutex::new(None),
            poll_lock: tokio::sync::Mutex::new(()),
            synced: AtomicBool::new(false),
            last_sync_attempt: Mutex::new(None),
            poll_blocked: AtomicBool::new(false),
            poll_now: tokio::sync::Notify::new(),
            upstream_requests: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            integrity_seen: AtomicU64::new(0),
            jitter: AtomicU64::new(0),
            shutdown: CancellationToken::new(),
            cfg,
        }
    }

    // ---- time ----

    /// The daemon clock as an `Instant` for the cache primitives: a fixed base plus the
    /// elapsed [`Clock`] time, so a `ManualClock` drives TTLs and staleness in tests.
    pub fn now(&self) -> Instant {
        let d = self
            .clock
            .now()
            .duration_since(self.base_time)
            .unwrap_or_default();
        self.base_instant
            .checked_add(d)
            .unwrap_or(self.base_instant)
    }

    fn access_ttl(&self) -> Duration {
        Duration::from_millis(self.cfg.access_cache_ttl_ms)
    }

    /// SPEC 13.3: no successful redaction refresh for longer than
    /// `redaction_max_staleness_ms` means fail-closed (forward every read).
    pub fn fail_closed_now(&self) -> bool {
        self.redactions.is_stale(
            self.now(),
            Duration::from_millis(self.cfg.redaction_max_staleness_ms),
        )
    }

    // ---- upstream ----

    /// The upstream client, dialing on first use and after a transport failure (SPEC 16.1:
    /// recovery needs no restart).
    pub async fn client(&self) -> Result<ScionClient> {
        let mut g = self.client.lock().await;
        if let Some(c) = g.as_ref() {
            return Ok(c.clone());
        }
        let c = self.upstream.connect().await?;
        *g = Some(c.clone());
        Ok(c)
    }

    /// Forget the connection after a transport-level failure (no ErrorDetail).
    pub async fn reset_client_on(&self, st: &tonic::Status) {
        if st.code() == tonic::Code::Unavailable && st.details().is_empty() {
            *self.client.lock().await = None;
        }
    }

    /// Forget the connection unconditionally (forwarding transport errors).
    pub async fn reset_client(&self) {
        *self.client.lock().await = None;
    }

    pub fn count_upstream(&self) {
        self.upstream_requests.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .upstream_requests
            .with_label_values(&[TIER])
            .inc();
    }

    /// Jittered exponential backoff before attempt `attempt + 1` (SPEC 10.5).
    pub async fn backoff(&self, attempt: u32) {
        let base = BACKOFF_BASE.saturating_mul(1u32 << attempt.saturating_sub(1).min(8));
        let r = splitmix(self.jitter.fetch_add(1, Ordering::Relaxed));
        // Jitter factor in [0.5, 1.5).
        let factor = 0.5 + (r >> 11) as f64 / (1u64 << 53) as f64;
        tokio::time::sleep(base.mul_f64(factor)).await;
    }

    /// One unary upstream call with at most [`MAX_ATTEMPTS`] attempts on UNAVAILABLE
    /// (SPEC 10.5). Epochs are observed from the response (or error) metadata.
    pub async fn unary<T, F, Fut>(&self, f: F) -> Result<T>
    where
        F: Fn(ScionClient) -> Fut,
        Fut: Future<Output = std::result::Result<tonic::Response<T>, tonic::Status>>,
    {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let err = match self.client().await {
                Err(e) => e,
                Ok(c) => {
                    self.count_upstream();
                    match f(c).await {
                        Ok(resp) => {
                            self.observe_metadata(resp.metadata());
                            return Ok(resp.into_inner());
                        }
                        Err(st) => {
                            self.observe_metadata(st.metadata());
                            self.reset_client_on(&st).await;
                            error_from_status(&st)
                        }
                    }
                }
            };
            if err.code == ErrorCode::Unavailable && attempt < MAX_ATTEMPTS {
                self.backoff(attempt).await;
                continue;
            }
            return Err(err);
        }
    }

    // ---- epochs ----

    pub fn observe_metadata(&self, md: &tonic::metadata::MetadataMap) {
        if let Some(e) = read_epochs(&md.clone().into_headers()) {
            self.observe_epochs(e);
        }
    }

    /// React to epochs seen on an upstream response (SPEC 10.4, 13.3): a different policy
    /// epoch drops every access decision and class; a higher redaction epoch triggers an
    /// immediate redaction poll.
    pub fn observe_epochs(&self, e: Epochs) {
        {
            let mut cur = self.policy_epoch.lock();
            if *cur != Some(e.policy) {
                if cur.is_some() {
                    tracing::info!(old = ?*cur, new = e.policy, "policy epoch changed; dropping access decisions");
                }
                self.access.on_policy_epoch(e.policy);
                self.classes.lock().clear();
                self.ref_readers.lock().clear();
                self.metrics.set_policy_epoch(e.policy);
                *cur = Some(e.policy);
            }
        }
        self.upstream_redaction_epoch
            .fetch_max(e.redaction, Ordering::Relaxed);
        if e.redaction > self.redactions.epoch() {
            self.poll_now.notify_one();
        }
    }

    pub fn policy_epoch(&self) -> Option<u64> {
        *self.policy_epoch.lock()
    }

    pub fn upstream_redaction_epoch(&self) -> u64 {
        self.upstream_redaction_epoch.load(Ordering::Relaxed)
    }

    // ---- access classes and decisions ----

    pub fn class_of(&self, id: &ObjectId) -> Option<i32> {
        self.classes.lock().get(id).copied()
    }

    pub fn set_class(&self, id: ObjectId, class: i32) {
        let mut c = self.classes.lock();
        if c.len() >= MAX_CLASSES {
            c.clear();
        }
        c.insert(id, class);
    }

    /// Record a positive per-(principal, object) decision learned from upstream (SPEC 10.4).
    pub fn record_allowed(&self, principal: &str, id: ObjectId, class: i32) {
        self.access.put(principal, &id, true, self.now());
        self.set_class(id, class);
    }

    pub fn note_ref_reader(&self, principal: &str) {
        let mut m = self.ref_readers.lock();
        if m.len() >= MAX_CLASSES {
            m.clear();
        }
        m.insert(principal.to_owned(), self.now());
    }

    /// True when `principal` may be served refs from the RefCache: confirmed root reader or a
    /// successful upstream ResolveRef of its own within the access TTL (SPEC 10.3, 10.4).
    pub fn ref_reader(&self, principal: &str) -> bool {
        let now = self.now();
        if self.access.root_reader(principal, now) == Some(true) {
            return true;
        }
        self.ref_readers
            .lock()
            .get(principal)
            .is_some_and(|at| now.saturating_duration_since(*at) < self.access_ttl())
    }

    // ---- tokens ----

    /// The verifier, loading the keys from upstream GetServerInfo on first use (SPEC 10.4).
    async fn verifier(&self) -> Result<Verifier> {
        if let Some(v) = self.verifier.read().clone() {
            return Ok(v);
        }
        self.refresh_server_info().await?;
        self.verifier
            .read()
            .clone()
            .ok_or_else(|| ScionError::unavailable(subcode::UPSTREAM_UNAVAILABLE, "no token keys"))
    }

    /// Verify `token` locally with the cached keys only (no upstream call).
    pub fn verify_cached(&self, token: &str) -> Option<String> {
        let v = self.verifier.read().clone()?;
        let claims = v.verify(token, self.clock.now()).ok()?;
        self.remember_token(token, claims.exp);
        Some(claims.sub.id_string())
    }

    /// Authenticate a request locally (SPEC 10.4, 12.1): signature and expiry against the keys
    /// from GetServerInfo, refreshed once when the token names an unknown key.
    pub async fn authenticate(&self, h: &http::HeaderMap) -> Result<Caller> {
        let token = bearer(h).ok_or_else(|| ScionError::unauthenticated("missing bearer token"))?;
        let v = self.verifier().await?;
        let now = self.clock.now();
        let claims = match v.verify(&token, now) {
            Ok(c) => c,
            Err(e) if e.message.contains("unknown key") && self.may_refresh_keys() => {
                self.refresh_server_info().await?;
                self.verifier().await?.verify(&token, now)?
            }
            Err(e) => return Err(e),
        };
        self.remember_token(&token, claims.exp);
        Ok(Caller {
            token,
            principal: claims.sub.id_string(),
        })
    }

    fn may_refresh_keys(&self) -> bool {
        let mut last = self.keys_refreshed_at.lock();
        let now = Instant::now();
        match *last {
            Some(t) if now.saturating_duration_since(t) < KEY_REFRESH_MIN => false,
            _ => {
                *last = Some(now);
                true
            }
        }
    }

    fn remember_token(&self, token: &str, exp: SystemTime) {
        let mut g = self.poll_token.lock();
        let replace = match g.as_ref() {
            None => true,
            Some((_, cur_exp)) => exp >= *cur_exp || *cur_exp <= self.clock.now(),
        };
        if replace {
            *g = Some((token.to_owned(), exp));
        }
    }

    /// A still-valid remembered token for background polls.
    pub fn poll_token(&self) -> Option<String> {
        let g = self.poll_token.lock();
        let (t, exp) = g.as_ref()?;
        (*exp > self.clock.now()).then(|| t.clone())
    }

    /// May a request run a synchronous initial sync now (rate-limited)?
    pub fn may_attempt_sync(&self) -> bool {
        let mut last = self.last_sync_attempt.lock();
        let now = Instant::now();
        match *last {
            Some(t) if now.saturating_duration_since(t) < SYNC_RETRY_MIN => false,
            _ => {
                *last = Some(now);
                true
            }
        }
    }

    // ---- server info ----

    /// Fetch GetServerInfo upstream; refresh the verification keys and the cached info.
    pub async fn refresh_server_info(&self) -> Result<v1::ServerInfo> {
        let info = self
            .unary(|c| async move {
                c.objects()
                    .get_server_info(v1::GetServerInfoRequest {})
                    .await
            })
            .await?;
        let keys: Vec<PublicKeyInfo> = info
            .token_public_keys
            .iter()
            .filter_map(|k| match scion_proto::convert::public_key_from_proto(k) {
                Ok((kid, ed25519_public)) => Some(PublicKeyInfo {
                    kid,
                    ed25519_public,
                }),
                Err(e) => {
                    tracing::warn!(error = %e, "ignoring a malformed upstream token key");
                    None
                }
            })
            .collect();
        *self.verifier.write() = Some(Verifier::new(keys));
        *self.server_info.lock() = Some((info.clone(), self.now()));
        Ok(info)
    }

    /// The cached GetServerInfo answer and when it was fetched.
    pub fn cached_server_info(&self) -> Option<(v1::ServerInfo, Instant)> {
        self.server_info.lock().clone()
    }

    pub fn repo(&self) -> String {
        self.server_info
            .lock()
            .as_ref()
            .map(|(i, _)| i.repo.clone())
            .unwrap_or_default()
    }

    // ---- metrics ----

    /// Mirror the cache's integrity-failure count into `scion_integrity_failures_total`
    /// (PLAN 6.10; the cache itself logs the alert).
    pub fn sync_cache_metrics(&self) {
        let now = self.cache.integrity_failures();
        let prev = self.integrity_seen.swap(now, Ordering::Relaxed);
        if now > prev {
            self.metrics
                .integrity_failures
                .with_label_values(&[TIER])
                .inc_by(now - prev);
        }
        self.metrics
            .cache_bytes
            .with_label_values(&[TIER])
            .set(i64::try_from(self.cache.usage()).unwrap_or(i64::MAX));
    }

    pub fn count_cache(&self, id: &ObjectId, hit: bool) {
        if hit {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.cache_misses.fetch_add(1, Ordering::Relaxed);
        }
        self.metrics
            .cache_requests
            .with_label_values(&[
                TIER,
                kind_label(id.kind()),
                if hit { "hit" } else { "miss" },
            ])
            .inc();
    }

    /// Emit the SPEC 15.1 RPC log event (tier daemon) and the duration histogram.
    pub fn log_rpc(
        &self,
        rpc: &str,
        request_id: &str,
        principal: Option<String>,
        status: &str,
        started: Instant,
    ) {
        let elapsed = started.elapsed();
        RpcLogEvent {
            request_id: request_id.to_owned(),
            rpc: rpc.to_owned(),
            tier: TIER,
            principal,
            repo: self.repo(),
            status: status.to_owned(),
            duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            ..Default::default()
        }
        .emit();
        self.metrics
            .rpc_duration
            .with_label_values(&[rpc, TIER, status])
            .observe(elapsed.as_secs_f64());
    }
}

/// The epochs stamped on daemon-served responses (SPEC 7.1, PLAN 6.13): the latest known
/// policy epoch, the local redaction epoch and tier `daemon`.
pub(crate) struct DaemonEpochs(pub Arc<Inner>);

impl EpochSource for DaemonEpochs {
    fn epochs(&self) -> Epochs {
        Epochs {
            policy: self.0.policy_epoch().unwrap_or(0),
            redaction: self.0.redactions.epoch(),
            tier: Tier::Daemon,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_parsing() {
        let mut h = http::HeaderMap::new();
        assert_eq!(bearer(&h), None);
        h.insert("authorization", "Bearer abc".parse().unwrap());
        assert_eq!(bearer(&h).as_deref(), Some("abc"));
        h.insert("authorization", "Basic abc".parse().unwrap());
        assert_eq!(bearer(&h), None);
        h.insert("authorization", "Bearer ".parse().unwrap());
        assert_eq!(bearer(&h), None);
    }

    #[test]
    fn jitter_is_spread() {
        let a = splitmix(1);
        let b = splitmix(2);
        assert_ne!(a, b);
    }
}
