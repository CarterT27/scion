//! Application wiring (SPEC 3.1; PLAN 1.4; P23).
//!
//! [`ServerApp::build`] assembles the PLAN 1.4 component graph over a [`Storage`] bundle,
//! bootstraps the repository and starts the background tasks. [`ServerApp::router`] wraps each
//! of the six `scion.v1` services in the layer stack (outermost first):
//!
//! 1. request id ([`crate::interceptor::RequestIdService`]),
//! 2. [`EpochTrailerLayer`] with the [`AppContext`] as epoch source, so every response,
//!    including rejections by the layers below, carries the three SPEC 7.1 trailers,
//! 3. RPC log events and `scion_rpc_duration_seconds` ([`crate::logging::LogService`]), so
//!    rejected calls are logged too,
//! 4. load shedding (tower `ConcurrencyLimit` + `LoadShed`, RESOURCE_EXHAUSTED),
//! 5. the bearer interceptor (UNAUTHENTICATED; inserts [`crate::CallerCtx`]),
//! 6. the per-principal governor limiter (RESOURCE_EXHAUSTED),
//!
//! and then the generated tonic service from `grpc.rs`.

use crate::authz::ObjectAuthorizer;
use crate::grpc::GrpcServices;
use crate::interceptor::{AuthService, BoxSvc, HttpReq, HttpResp, RequestIdService};
use crate::logging::LogService;
use crate::ratelimit::{GovernorService, Limits, ShedService};
use crate::redaction_set::RedactionSet;
use scion_auth::{GitHubClient, Keyring, ServicePrincipals, Verifier};
use scion_config::ServerConfig;
use scion_core::{Clock, ErrorCode, Principal, Result, ScionError, Tier, subcode};
use scion_derived::{DeriveConfig, DeriveMetrics, DeriveMode, DerivedEngine, Registry};
use scion_history::{
    EagerHook, History, HistoryConfig, LandAttemptOutcome, LandConfig, LandObserver,
};
use scion_object::RefName;
use scion_obs::{Metrics, Readiness};
use scion_policy::PolicyHandle;
use scion_proto::{EpochSource, EpochTrailerLayer, Epochs, v1};
use scion_redaction::{RedactionConfig, Redactor};
use scion_store::{ObjectRepo, ObjectRepoConfig, Storage};
use std::convert::Infallible;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use tower::util::BoxCloneSyncService;
use tower_layer::Layer;
use tower_service::Service;

/// How long [`ServerHandle::shutdown`] waits for in-flight calls before forcing connections
/// closed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// Upper bound on one readiness probe of a dependency (SPEC 15.3).
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Extra decoding headroom above `limits.max_blob_bytes` for request messages (an upload item
/// carries one blob plus framing).
const DECODE_HEADROOM: u64 = 16 * 1024 * 1024;

/// Inputs of [`ServerApp::build`] (PLAN 1.4).
pub struct ServerDeps {
    /// Storage bundle (CRDB+S3 in production, in-memory in tests).
    pub storage: Storage,
    /// Parsed server configuration.
    pub config: ServerConfig,
    /// Loaded policy.
    pub policy: Arc<PolicyHandle>,
    /// Token signing keys.
    pub keyring: Keyring,
    /// GitHub API client.
    pub github: GitHubClient,
    /// Time source.
    pub clock: Arc<dyn Clock>,
    /// Metrics registry.
    pub metrics: Arc<Metrics>,
    /// Derived type registry; `None` = `Registry::v1_default()`.
    pub registry: Option<Registry>,
}

/// Everything a handler needs (PLAN 1.4, 2.14).
pub struct AppContext {
    /// Storage bundle.
    pub storage: Storage,
    /// Object facade.
    pub objects: ObjectRepo,
    /// History engine.
    pub history: Arc<History>,
    /// Derived data engine.
    pub derived: Arc<DerivedEngine>,
    /// Redaction lifecycle.
    pub redactor: Arc<Redactor>,
    /// In-memory redaction set (SPEC 13.3).
    pub redactions: Arc<RedactionSet>,
    /// Policy.
    pub policy: Arc<PolicyHandle>,
    /// Token keys.
    pub keyring: Keyring,
    /// Token verifier.
    pub verifier: Verifier,
    /// Service principals.
    pub services: ServicePrincipals,
    /// GitHub API client.
    pub github: GitHubClient,
    /// Time source.
    pub clock: Arc<dyn Clock>,
    /// Metrics.
    pub metrics: Arc<Metrics>,
    /// Configuration.
    pub config: ServerConfig,
    /// Object read authorization (SPEC 18.6).
    pub authz: ObjectAuthorizer,
}

impl EpochSource for AppContext {
    fn epochs(&self) -> Epochs {
        Epochs {
            policy: self.policy.epoch(),
            redaction: self.redactions.epoch(),
            tier: Tier::Server,
        }
    }
}

/// `scion_land_attempts{result}`, `scion_ref_cas_failures_total{ref}` and
/// `scion_land_duration_seconds` from land attempts (SPEC 15.2).
pub struct MetricsLandObserver {
    metrics: Arc<Metrics>,
}

impl MetricsLandObserver {
    /// An observer recording into `metrics`.
    pub fn new(metrics: Arc<Metrics>) -> Self {
        MetricsLandObserver { metrics }
    }

    /// The `result` label of an outcome.
    pub fn result_label(outcome: &LandAttemptOutcome) -> &'static str {
        match outcome {
            LandAttemptOutcome::Committed => "committed",
            LandAttemptOutcome::Idempotent => "idempotent",
            LandAttemptOutcome::CasMismatch => "cas_mismatch",
            LandAttemptOutcome::LandedConflict => "landed_conflict",
            LandAttemptOutcome::Failed(_) => "failed",
        }
    }
}

impl LandObserver for MetricsLandObserver {
    fn attempt(
        &self,
        ref_name: &RefName,
        _attempt: u32,
        outcome: &LandAttemptOutcome,
        elapsed: Duration,
    ) {
        self.metrics
            .land_attempts
            .with_label_values(&[Self::result_label(outcome)])
            .inc();
        match outcome {
            LandAttemptOutcome::CasMismatch => {
                self.metrics
                    .ref_cas_failures
                    .with_label_values(&[ref_name.as_str()])
                    .inc();
            }
            // Retried; the land is not over yet.
            LandAttemptOutcome::LandedConflict => {}
            // Terminal outcomes: the land's total latency (all attempts included).
            LandAttemptOutcome::Committed
            | LandAttemptOutcome::Idempotent
            | LandAttemptOutcome::Failed(_) => {
                self.metrics.land_duration.observe(elapsed.as_secs_f64());
            }
        }
    }
}

/// `scion_derive_duration_seconds{type,mode}` and `scion_derive_backlog{type}` (SPEC 15.2).
pub struct PrometheusDeriveMetrics {
    metrics: Arc<Metrics>,
}

impl PrometheusDeriveMetrics {
    /// An adapter recording into `metrics`.
    pub fn new(metrics: Arc<Metrics>) -> Self {
        PrometheusDeriveMetrics { metrics }
    }
}

impl DeriveMetrics for PrometheusDeriveMetrics {
    fn derive_duration(&self, ty: &str, mode: DeriveMode, d: Duration) {
        let mode = match mode {
            DeriveMode::Eager => "eager",
            DeriveMode::Lazy => "lazy",
        };
        self.metrics
            .derive_duration
            .with_label_values(&[ty, mode])
            .observe(d.as_secs_f64());
    }

    fn backlog(&self, ty: &str, delta: i64) {
        self.metrics
            .derive_backlog
            .with_label_values(&[ty])
            .add(delta);
    }
}

fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}

/// A built server (PLAN 1.4).
pub struct ServerApp {
    ctx: Arc<AppContext>,
    limits: Arc<Limits>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for ServerApp {
    fn drop(&mut self) {
        // Tasks not handed to a ServerHandle belong to an app that never served.
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl ServerApp {
    /// Bootstrap the repository and wire every component, background task and the policy
    /// watcher (PLAN 1.4; SPEC 6.5 bootstrap, 13.3 refresher, 6.2 reload).
    pub async fn build(deps: ServerDeps) -> Result<Self> {
        let ServerDeps {
            storage,
            config,
            policy,
            keyring,
            github,
            clock,
            metrics,
            registry,
        } = deps;
        let objects = ObjectRepo::new(
            storage.clone(),
            ObjectRepoConfig {
                prefix: config.objects.prefix.clone(),
                inline_threshold_bytes: config.objects.inline_threshold_bytes,
                zstd_level: config.objects.zstd_level,
                max_blob_bytes: config.limits.max_blob_bytes,
                ..ObjectRepoConfig::default()
            },
        );
        let derived = DerivedEngine::new(
            objects.clone(),
            registry.unwrap_or_else(Registry::v1_default),
            DeriveConfig {
                lease_ttl: ms(config.derive.lease_ttl_ms),
                lazy_workers: config.derive.lazy_workers,
                holder_id: format!("scion-server-{}", uuid::Uuid::new_v4()),
            },
            clock.clone(),
        );
        derived.set_metrics(Arc::new(PrometheusDeriveMetrics::new(metrics.clone())));
        let eager: Arc<dyn EagerHook> = derived.clone();
        let history = Arc::new(
            History::new(
                objects.clone(),
                HistoryConfig {
                    limits: config.limits.clone(),
                    land: LandConfig {
                        max_attempts: config.land.max_attempts,
                        backoff_base: ms(config.land.backoff_base_ms),
                        backoff_max: ms(config.land.backoff_max_ms),
                        rng_seed: None,
                    },
                    bootstrap_principal: Principal::Service("scion".to_owned()),
                },
                eager,
                clock.clone(),
            )
            .with_land_observer(Arc::new(MetricsLandObserver::new(metrics.clone()))),
        );
        let propagation_bound = ms(config.redaction.propagation_bound_ms);
        let redactor = Redactor::new(
            objects.clone(),
            derived.clone(),
            clock.clone(),
            RedactionConfig {
                propagation_bound,
                default_quarantine: config.redaction.default_quarantine,
                ..RedactionConfig::default()
            },
        );
        let redactions = RedactionSet::new(storage.meta.clone());
        redactions.set_metrics(metrics.clone());
        let services = ServicePrincipals::new(
            config
                .auth
                .service_principals
                .iter()
                .map(|s| (s.name.clone(), s.secret_sha256.clone()))
                .collect(),
        )?;
        let verifier = keyring.verifier();
        let authz = ObjectAuthorizer::new(storage.clone(), policy.clone(), redactions.clone());

        history.bootstrap().await?;
        redactions.refresh().await?;

        let limits = Limits::new(&config.rate_limit);
        let reload = config.policy.reload;
        let ctx = Arc::new(AppContext {
            storage,
            objects,
            history,
            derived,
            redactor,
            redactions: redactions.clone(),
            policy,
            keyring,
            verifier,
            services,
            github,
            clock,
            metrics: metrics.clone(),
            config,
            authz,
        });
        let mut tasks = vec![redactions.spawn_refresher(propagation_bound)];
        tasks.extend(crate::reload::spawn(&ctx.policy, metrics, reload));
        crate::handlers::admin::spawn_background(&ctx);
        tracing::info!(
            repo = ctx.config.server.repo.as_str(),
            policy_epoch = ctx.policy.epoch(),
            redaction_epoch = ctx.redactions.epoch(),
            "scion-server built"
        );
        Ok(ServerApp { ctx, limits, tasks })
    }

    /// The application context.
    pub fn context(&self) -> Arc<AppContext> {
        self.ctx.clone()
    }

    /// The gRPC router: the six services plus every layer (PLAN 1.4).
    pub fn router(&self) -> tonic::transport::server::Router {
        self.build_router(tonic::transport::Server::builder())
    }

    fn build_router(
        &self,
        mut builder: tonic::transport::Server,
    ) -> tonic::transport::server::Router {
        let svcs = GrpcServices::new(self.ctx.clone());
        let max_decode = usize::try_from(
            self.ctx
                .config
                .limits
                .max_blob_bytes
                .saturating_add(DECODE_HEADROOM),
        )
        .unwrap_or(usize::MAX);
        let stack = Stack {
            ctx: self.ctx.clone(),
            limits: self.limits.clone(),
        };
        builder
            .add_service(
                stack.wrap(
                    v1::object_service_server::ObjectServiceServer::new(svcs.clone())
                        .max_decoding_message_size(max_decode),
                ),
            )
            .add_service(
                stack.wrap(
                    v1::query_service_server::QueryServiceServer::new(svcs.clone())
                        .max_decoding_message_size(max_decode),
                ),
            )
            .add_service(
                stack.wrap(
                    v1::write_service_server::WriteServiceServer::new(svcs.clone())
                        .max_decoding_message_size(max_decode),
                ),
            )
            .add_service(
                stack.wrap(
                    v1::ref_service_server::RefServiceServer::new(svcs.clone())
                        .max_decoding_message_size(max_decode),
                ),
            )
            .add_service(
                stack.wrap(
                    v1::auth_service_server::AuthServiceServer::new(svcs.clone())
                        .max_decoding_message_size(max_decode),
                ),
            )
            .add_service(
                stack.wrap(
                    v1::admin_service_server::AdminServiceServer::new(svcs)
                        .max_decoding_message_size(max_decode),
                ),
            )
    }

    /// Serve on `listener`, with TLS when `tls = Some((cert_pem, key_pem))`, plus the admin
    /// listener on `observability.metrics_listen` (SPEC 7.1, 15.3, 17).
    pub async fn serve(
        self,
        listener: tokio::net::TcpListener,
        tls: Option<(Vec<u8>, Vec<u8>)>,
    ) -> Result<ServerHandle> {
        self.serve_with(listener, tls, None).await
    }

    /// [`ServerApp::serve`], additionally serving plaintext gRPC on a Unix socket at `unix`
    /// (in-process tests without TLS).
    pub(crate) async fn serve_with(
        mut self,
        listener: tokio::net::TcpListener,
        tls: Option<(Vec<u8>, Vec<u8>)>,
        unix: Option<PathBuf>,
    ) -> Result<ServerHandle> {
        let io_err = |what: &str, e: std::io::Error| {
            ScionError::unavailable(subcode::STORE_UNAVAILABLE, format!("{what}: {e}"))
        };
        let addr = listener
            .local_addr()
            .map_err(|e| io_err("listener address", e))?;
        let builder = match &tls {
            Some((cert, key)) => tonic::transport::Server::builder()
                .tls_config(crate::tls::tonic_config(cert, key)?)
                .map_err(|e| {
                    ScionError::new(
                        ErrorCode::InvalidArgument,
                        format!("TLS configuration: {e}"),
                    )
                })?,
            None => tonic::transport::Server::builder(),
        };
        let router = self.build_router(builder);

        // Admin listener first, so a bind failure leaves nothing running.
        let ready = Arc::new(Ready::new(self.ctx.clone()));
        let admin_router = scion_obs::health_router(self.ctx.metrics.clone(), ready.clone())
            .layer(ReadyProbeLayer { ready });
        let (admin_addr, admin_task) =
            scion_obs::serve_admin(self.ctx.config.observability.metrics_listen, admin_router)
                .await
                .map_err(|e| io_err("admin listener", e))?;

        let unix_listener = match &unix {
            Some(path) => {
                let _ = std::fs::remove_file(path);
                Some(tokio::net::UnixListener::bind(path).map_err(|e| io_err("unix listener", e))?)
            }
            None => None,
        };

        let kill = CancellationToken::new();
        let graceful = CancellationToken::new();
        let mut serving = Vec::new();
        {
            let kill = kill.clone();
            let incoming = futures::StreamExt::map(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                move |r| {
                    r.map(|s| {
                        let _ = s.set_nodelay(true);
                        KillableIo::new(s, &kill)
                    })
                },
            );
            let signal = graceful.clone().cancelled_owned();
            serving.push(tokio::spawn(async move {
                if let Err(e) = router.serve_with_incoming_shutdown(incoming, signal).await {
                    tracing::error!(error = %e, "gRPC listener stopped");
                }
            }));
        }
        if let Some(ul) = unix_listener {
            let kill = kill.clone();
            let router = self.build_router(tonic::transport::Server::builder());
            let incoming = futures::StreamExt::map(
                tokio_stream::wrappers::UnixListenerStream::new(ul),
                move |r| r.map(|s| KillableIo::new(s, &kill)),
            );
            let signal = graceful.clone().cancelled_owned();
            serving.push(tokio::spawn(async move {
                if let Err(e) = router.serve_with_incoming_shutdown(incoming, signal).await {
                    tracing::error!(error = %e, "gRPC unix listener stopped");
                }
            }));
        }
        let mut tasks = std::mem::take(&mut self.tasks);
        tasks.push(admin_task);
        tracing::info!(%addr, %admin_addr, tls = tls.is_some(), "scion-server serving");
        Ok(ServerHandle {
            addr,
            admin_addr: Some(admin_addr),
            tasks,
            serving,
            kill,
            graceful,
        })
    }
}

/// Applies the layer stack to one generated service.
#[derive(Clone)]
struct Stack {
    ctx: Arc<AppContext>,
    limits: Arc<Limits>,
}

impl Stack {
    fn wrap<S>(&self, svc: S) -> Layered<S>
    where
        S: Service<HttpReq, Response = HttpResp, Error = Infallible>
            + tonic::server::NamedService
            + Clone
            + Send
            + Sync
            + 'static,
        S::Future: Send + 'static,
    {
        let ctx = &self.ctx;
        let s: BoxSvc = BoxCloneSyncService::new(svc);
        let s = BoxCloneSyncService::new(GovernorService::new(s, self.limits.clone()));
        let s =
            BoxCloneSyncService::new(AuthService::new(s, ctx.verifier.clone(), ctx.clock.clone()));
        let s = BoxCloneSyncService::new(ShedService::new(s, &self.limits));
        let s = BoxCloneSyncService::new(LogService::new(
            s,
            ctx.metrics.clone(),
            &ctx.config.server.repo,
        ));
        let epochs: Arc<dyn EpochSource> = ctx.clone();
        let s = EpochTrailerLayer::new(epochs)
            .layer(s)
            .map_response(|r: http::Response<_>| r.map(tonic::body::Body::new));
        let s = BoxCloneSyncService::new(RequestIdService::new(BoxCloneSyncService::new(s)));
        Layered {
            inner: s,
            _named: PhantomData,
        }
    }
}

/// A layered service that keeps the gRPC service name of `S` for routing.
struct Layered<S> {
    inner: BoxSvc,
    _named: PhantomData<fn() -> S>,
}

impl<S> Clone for Layered<S> {
    fn clone(&self) -> Self {
        Layered {
            inner: self.inner.clone(),
            _named: PhantomData,
        }
    }
}

impl<S: tonic::server::NamedService> tonic::server::NamedService for Layered<S> {
    const NAME: &'static str = S::NAME;
}

impl<S> Service<HttpReq> for Layered<S> {
    type Response = HttpResp;
    type Error = Infallible;
    type Future = crate::interceptor::BoxFut;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: HttpReq) -> Self::Future {
        Box::pin(self.inner.call(req))
    }
}

pin_project_lite::pin_project! {
    /// A connection that fails every read and write once the server is killed, so killed
    /// servers drop their connections like a dead process would.
    struct KillableIo<IO> {
        #[pin]
        io: IO,
        killed: Pin<Box<tokio_util::sync::WaitForCancellationFutureOwned>>,
    }
}

impl<IO> KillableIo<IO> {
    fn new(io: IO, kill: &CancellationToken) -> Self {
        KillableIo {
            io,
            killed: Box::pin(kill.clone().cancelled_owned()),
        }
    }
}

fn killed_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "server killed")
}

impl<IO: tokio::io::AsyncRead> tokio::io::AsyncRead for KillableIo<IO> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.project();
        if this.killed.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(killed_error()));
        }
        this.io.poll_read(cx, buf)
    }
}

impl<IO: tokio::io::AsyncWrite> tokio::io::AsyncWrite for KillableIo<IO> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.project();
        if this.killed.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(killed_error()));
        }
        this.io.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.project();
        if this.killed.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(killed_error()));
        }
        this.io.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.project().io.poll_shutdown(cx)
    }
}

impl<IO: tonic::transport::server::Connected> tonic::transport::server::Connected
    for KillableIo<IO>
{
    type ConnectInfo = IO::ConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.io.connect_info()
    }
}

use std::future::Future as _;

/// SPEC 15.3 readiness: the metadata store answers `ping`, the object store answers a probe,
/// and a policy is loaded. Probed on every `/readyz` request by [`ReadyProbeLayer`] (no
/// background traffic, so storage counters and fault hooks stay quiet otherwise).
struct Ready {
    ctx: Arc<AppContext>,
    state: parking_lot::Mutex<std::result::Result<(), String>>,
}

impl Ready {
    fn new(ctx: Arc<AppContext>) -> Self {
        Ready {
            ctx,
            state: parking_lot::Mutex::new(Ok(())),
        }
    }

    async fn probe(&self) {
        let storage = &self.ctx.storage;
        let meta = tokio::time::timeout(READY_PROBE_TIMEOUT, storage.meta.ping()).await;
        let result = match meta {
            Err(_) => Err("metadata store: ping timed out".to_owned()),
            Ok(Err(e)) => Err(format!("metadata store: {e}")),
            Ok(Ok(())) => {
                let key = format!("{}/readyz-probe", self.ctx.config.objects.prefix);
                match tokio::time::timeout(READY_PROBE_TIMEOUT, storage.blobs.exists(&key)).await {
                    Err(_) => Err("object store: probe timed out".to_owned()),
                    Ok(Err(e)) => Err(format!("object store: {e}")),
                    Ok(Ok(_)) => Ok(()),
                }
            }
        };
        *self.state.lock() = result;
    }
}

impl Readiness for Ready {
    fn check(&self) -> std::result::Result<(), String> {
        self.state.lock().clone()?;
        // The handle always holds the last known good policy (startup fails without one).
        let _loaded = self.ctx.policy.current();
        Ok(())
    }
}

/// Runs [`Ready::probe`] before `/readyz` is answered.
#[derive(Clone)]
struct ReadyProbeLayer {
    ready: Arc<Ready>,
}

impl<S> Layer<S> for ReadyProbeLayer {
    type Service = ReadyProbe<S>;

    fn layer(&self, inner: S) -> ReadyProbe<S> {
        ReadyProbe {
            inner,
            ready: self.ready.clone(),
        }
    }
}

#[derive(Clone)]
struct ReadyProbe<S> {
    inner: S,
    ready: Arc<Ready>,
}

impl<S, B> Service<http::Request<B>> for ReadyProbe<S>
where
    S: Service<http::Request<B>, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Response: Send + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<S::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        if req.uri().path() != "/readyz" {
            return Box::pin(self.inner.call(req));
        }
        let ready = self.ready.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            ready.probe().await;
            inner.oneshot(req).await
        })
    }
}

/// A running server (PLAN 2.14).
pub struct ServerHandle {
    /// gRPC address.
    pub addr: SocketAddr,
    /// Admin (metrics/health) address.
    pub admin_addr: Option<SocketAddr>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    serving: Vec<tokio::task::JoinHandle<()>>,
    kill: CancellationToken,
    graceful: CancellationToken,
}

impl ServerHandle {
    /// Graceful shutdown: stop accepting, let in-flight calls finish (bounded), then stop
    /// every task.
    pub async fn shutdown(self) {
        self.graceful.cancel();
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        for t in self.serving.iter() {
            while !t.is_finished() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        self.abort();
        self.join().await;
    }

    /// Abort every task immediately; open connections fail at once.
    pub fn abort(&self) {
        self.kill.cancel();
        self.graceful.cancel();
        for t in self.serving.iter().chain(self.tasks.iter()) {
            t.abort();
        }
    }

    /// [`ServerHandle::abort`], then wait until every task has stopped (the listener is
    /// closed when this returns).
    pub(crate) async fn abort_and_wait(self) {
        self.abort();
        self.join().await;
    }

    async fn join(self) {
        for t in self.serving.into_iter().chain(self.tasks) {
            let _ = t.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn land_observer_maps_outcomes() {
        let m = Metrics::new();
        let obs = MetricsLandObserver::new(m.clone());
        let main = RefName::main();
        obs.attempt(&main, 1, &LandAttemptOutcome::CasMismatch, Duration::ZERO);
        obs.attempt(
            &main,
            2,
            &LandAttemptOutcome::LandedConflict,
            Duration::ZERO,
        );
        obs.attempt(
            &main,
            3,
            &LandAttemptOutcome::Committed,
            Duration::from_millis(5),
        );
        obs.attempt(
            &main,
            1,
            &LandAttemptOutcome::Failed(ErrorCode::MergeConflict),
            Duration::ZERO,
        );
        obs.attempt(&main, 1, &LandAttemptOutcome::Idempotent, Duration::ZERO);
        for (label, n) in [
            ("committed", 1),
            ("cas_mismatch", 1),
            ("landed_conflict", 1),
            ("failed", 1),
            ("idempotent", 1),
        ] {
            assert_eq!(
                m.land_attempts.with_label_values(&[label]).get(),
                n,
                "{label}"
            );
        }
        assert_eq!(m.ref_cas_failures.with_label_values(&["main"]).get(), 1);
        assert_eq!(m.land_duration.get_sample_count(), 3);
    }

    #[test]
    fn derive_metrics_adapter() {
        let m = Metrics::new();
        let d = PrometheusDeriveMetrics::new(m.clone());
        d.derive_duration("blame", DeriveMode::Lazy, Duration::from_millis(3));
        d.backlog("blame", 3);
        d.backlog("blame", -1);
        assert_eq!(
            m.derive_duration
                .with_label_values(&["blame", "lazy"])
                .get_sample_count(),
            1
        );
        assert_eq!(m.derive_backlog.with_label_values(&["blame"]).get(), 2);
    }
}
