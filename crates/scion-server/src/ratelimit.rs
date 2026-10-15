//! Load shedding and per-principal rate limits (SPEC 10.5; PLAN 6.11 `[rate_limit]`; P23).
//!
//! - [`ShedService`]: a concurrency limit over one semaphore shared by every service of the
//!   server (`rate_limit.max_inflight` permits) with load-shed semantics (tower
//!   `ConcurrencyLimit` + `LoadShed`): a call that finds no free permit fails
//!   RESOURCE_EXHAUSTED immediately instead of queueing, so the server sheds load before its
//!   dependencies saturate. Unlike tower's `ConcurrencyLimit`, which releases the permit when
//!   the response *headers* are ready, the permit is held until the response body (the whole
//!   stream of a streaming RPC such as `GetObjects`) has finished or been dropped.
//! - [`GovernorService`]: a governor keyed limiter, `rate_limit.per_principal_rps` with burst
//!   `rate_limit.burst`, keyed by the authenticated principal (unauthenticated calls are keyed by
//!   peer address). Exceeding it fails RESOURCE_EXHAUSTED.

use crate::context::CallerCtx;
use crate::errors::error_response;
use crate::interceptor::{BoxFut, BoxSvc, HttpReq, HttpResp};
use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};
use scion_core::ScionError;
use std::convert::Infallible;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower::ServiceExt;
use tower_service::Service;

/// How many limiter checks happen between purges of idle per-principal state.
const RETAIN_EVERY: u64 = 4096;

/// Ids one extra rate-limit token buys in a multi-id call (GetObjects, HasObjects): the call
/// itself paid one token at the RPC layer, and every further `IDS_PER_TOKEN` ids cost one
/// more, so a request for millions of ids cannot ride on a single token.
pub(crate) const IDS_PER_TOKEN: usize = 1000;

/// Shared limits of one server: the in-flight semaphore and the per-principal limiter.
pub struct Limits {
    inflight: Arc<Semaphore>,
    limiter: Option<DefaultKeyedRateLimiter<String>>,
    burst: NonZeroU32,
    checks: AtomicU64,
}

impl Limits {
    /// Limits from `[rate_limit]` (SPEC 10.5, PLAN 6.11). A zero `per_principal_rps` disables
    /// the per-principal limiter; a zero burst is treated as 1.
    pub fn new(cfg: &scion_config::RateLimitSection) -> Arc<Self> {
        let burst = NonZeroU32::new(cfg.burst).unwrap_or(NonZeroU32::MIN);
        let limiter = NonZeroU32::new(cfg.per_principal_rps)
            .map(|rps| RateLimiter::keyed(Quota::per_second(rps).allow_burst(burst)));
        Arc::new(Limits {
            inflight: Arc::new(Semaphore::new(cfg.max_inflight.min(Semaphore::MAX_PERMITS))),
            limiter,
            burst,
            checks: AtomicU64::new(0),
        })
    }

    /// Charge `key` for a multi-id call of `ids` ids beyond the one token the call already
    /// paid: one token per [`IDS_PER_TOKEN`] ids, capped at the burst (a larger charge could
    /// never succeed). False when the principal is over its rate (SPEC 10.5).
    pub fn charge_ids(&self, key: &str, ids: usize) -> bool {
        let Some(l) = &self.limiter else {
            return true;
        };
        let extra = u32::try_from(ids / IDS_PER_TOKEN).unwrap_or(u32::MAX);
        let Some(n) = NonZeroU32::new(extra.min(self.burst.get())) else {
            return true;
        };
        matches!(l.check_key_n(&key.to_owned(), n), Ok(Ok(())))
    }

    /// Whether a call keyed by `key` is within its rate (SPEC 10.5).
    pub fn check(&self, key: &str) -> bool {
        let Some(l) = &self.limiter else {
            return true;
        };
        if self.checks.fetch_add(1, Ordering::Relaxed) % RETAIN_EVERY == RETAIN_EVERY - 1 {
            l.retain_recent();
        }
        l.check_key(&key.to_owned()).is_ok()
    }

    /// Free in-flight permits.
    #[cfg(test)]
    pub fn available_permits(&self) -> usize {
        self.inflight.available_permits()
    }
}

/// Load shedding over the shared in-flight semaphore: a call that cannot take a permit at once
/// fails RESOURCE_EXHAUSTED (SPEC 10.5). The permit lives until the response body ends.
#[derive(Clone)]
pub struct ShedService {
    inner: BoxSvc,
    inflight: Arc<Semaphore>,
}

impl ShedService {
    /// Wrap `inner` with the shared in-flight limit of `limits`.
    pub fn new(inner: BoxSvc, limits: &Limits) -> Self {
        ShedService {
            inner,
            inflight: limits.inflight.clone(),
        }
    }
}

impl Service<HttpReq> for ShedService {
    type Response = HttpResp;
    type Error = Infallible;
    type Future = BoxFut;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: HttpReq) -> BoxFut {
        let Ok(permit) = self.inflight.clone().try_acquire_owned() else {
            let e = ScionError::resource_exhausted("server overloaded; retry later");
            return Box::pin(std::future::ready(Ok(error_response(&e))));
        };
        let inner = self.inner.clone();
        Box::pin(async move {
            let resp = inner.oneshot(req).await?;
            Ok(resp.map(|body| {
                tonic::body::Body::new(PermitBody {
                    inner: body,
                    permit: Some(permit),
                })
            }))
        })
    }
}

/// A response body holding an in-flight permit until its end (or until it is dropped).
struct PermitBody {
    inner: tonic::body::Body,
    permit: Option<OwnedSemaphorePermit>,
}

impl http_body::Body for PermitBody {
    type Data = bytes::Bytes;
    type Error = tonic::Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<bytes::Bytes>, tonic::Status>>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(None) | Poll::Ready(Some(Err(_))) => this.permit = None,
            Poll::Ready(Some(Ok(frame))) if frame.is_trailers() => this.permit = None,
            _ => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// The rate-limit key of a request: the principal, else `anonymous@<peer ip>`.
fn rate_key(req: &HttpReq) -> String {
    if let Some(c) = req.extensions().get::<CallerCtx>() {
        return c.principal.id_string();
    }
    use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};
    let peer = req
        .extensions()
        .get::<TcpConnectInfo>()
        .and_then(|i| i.remote_addr())
        .or_else(|| {
            req.extensions()
                .get::<TlsConnectInfo<TcpConnectInfo>>()
                .and_then(|i| i.get_ref().remote_addr())
        });
    match peer {
        Some(a) => format!("anonymous@{}", a.ip()),
        None => "anonymous".to_owned(),
    }
}

/// The per-principal limiter (SPEC 10.5): RESOURCE_EXHAUSTED beyond
/// `per_principal_rps`/`burst`.
#[derive(Clone)]
pub struct GovernorService {
    inner: BoxSvc,
    limits: Arc<Limits>,
}

impl GovernorService {
    /// Wrap `inner`.
    pub fn new(inner: BoxSvc, limits: Arc<Limits>) -> Self {
        GovernorService { inner, limits }
    }
}

impl Service<HttpReq> for GovernorService {
    type Response = HttpResp;
    type Error = Infallible;
    type Future = BoxFut;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: HttpReq) -> BoxFut {
        let key = rate_key(&req);
        if !self.limits.check(&key) {
            let e = ScionError::resource_exhausted("per-principal rate limit exceeded");
            return Box::pin(std::future::ready(Ok(error_response(&e))));
        }
        Box::pin(self.inner.clone().oneshot(req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_config::RateLimitSection;
    use std::time::Duration;
    use tower::util::BoxCloneSyncService;

    fn slow_service(delay: Duration) -> BoxSvc {
        BoxCloneSyncService::new(tower::service_fn(move |_req: HttpReq| async move {
            tokio::time::sleep(delay).await;
            Ok::<_, Infallible>(HttpResp::new(tonic::body::Body::empty()))
        }))
    }

    fn req() -> HttpReq {
        http::Request::builder()
            .uri("http://x/scion.v1.AuthService/WhoAmI")
            .body(tonic::body::Body::empty())
            .unwrap()
    }

    fn code(r: &HttpResp) -> Option<tonic::Code> {
        tonic::Status::from_header_map(r.headers()).map(|s| s.code())
    }

    #[tokio::test]
    async fn sheds_beyond_max_inflight() {
        let limits = Limits::new(&RateLimitSection {
            per_principal_rps: 0,
            burst: 0,
            max_inflight: 2,
        });
        let svc = ShedService::new(slow_service(Duration::from_millis(300)), &limits);
        let a = tokio::spawn(svc.clone().oneshot(req()));
        let b = tokio::spawn(svc.clone().oneshot(req()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(limits.available_permits(), 0);
        let c = svc.clone().oneshot(req()).await.unwrap();
        assert_eq!(code(&c), Some(tonic::Code::ResourceExhausted));
        assert_eq!(code(&a.await.unwrap().unwrap()), None);
        assert_eq!(code(&b.await.unwrap().unwrap()), None);
        // Capacity is back once the calls finished.
        let d = svc.oneshot(req()).await.unwrap();
        assert_eq!(code(&d), None);
        drop(d);
        assert_eq!(limits.available_permits(), 2);
    }

    /// A streaming response keeps its permit until the stream has ended, not just until the
    /// headers were sent.
    #[tokio::test]
    async fn streaming_body_holds_its_permit_until_the_end() {
        use http_body_util::BodyExt;
        let limits = Limits::new(&RateLimitSection {
            per_principal_rps: 0,
            burst: 0,
            max_inflight: 1,
        });
        let (tx, rx) =
            tokio::sync::mpsc::channel::<Result<http_body::Frame<bytes::Bytes>, tonic::Status>>(4);
        let rx = Arc::new(parking_lot::Mutex::new(Some(rx)));
        let streaming: BoxSvc =
            BoxCloneSyncService::new(tower::service_fn(move |_req: HttpReq| {
                let rx = rx.lock().take();
                async move {
                    let body = match rx {
                        Some(rx) => tonic::body::Body::new(http_body_util::StreamBody::new(
                            tokio_stream::wrappers::ReceiverStream::new(rx),
                        )),
                        None => tonic::body::Body::empty(),
                    };
                    Ok::<_, Infallible>(HttpResp::new(body))
                }
            }));
        let svc = ShedService::new(streaming, &limits);
        let resp = svc.clone().oneshot(req()).await.unwrap();
        assert_eq!(code(&resp), None);
        assert_eq!(limits.available_permits(), 0);
        // Headers are out but the stream is still open: no capacity.
        let shed = svc.clone().oneshot(req()).await.unwrap();
        assert_eq!(code(&shed), Some(tonic::Code::ResourceExhausted));
        drop(shed);

        let mut body = resp.into_body();
        tx.send(Ok(http_body::Frame::data(bytes::Bytes::from_static(b"x"))))
            .await
            .unwrap();
        let mut trailers = http::HeaderMap::new();
        trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
        tx.send(Ok(http_body::Frame::trailers(trailers)))
            .await
            .unwrap();
        drop(tx);
        assert!(body.frame().await.unwrap().unwrap().is_data());
        assert_eq!(limits.available_permits(), 0);
        assert!(body.frame().await.unwrap().unwrap().is_trailers());
        assert_eq!(limits.available_permits(), 1, "released at the trailers");
    }

    #[tokio::test]
    async fn governor_limits_per_key() {
        let limits = Limits::new(&RateLimitSection {
            per_principal_rps: 1,
            burst: 2,
            max_inflight: 10,
        });
        assert!(limits.check("a"));
        assert!(limits.check("a"));
        assert!(!limits.check("a"));
        assert!(limits.check("b"));
        // Multi-id calls pay one more token per IDS_PER_TOKEN ids, capped at the burst.
        let by_ids = Limits::new(&RateLimitSection {
            per_principal_rps: 1,
            burst: 3,
            max_inflight: 10,
        });
        assert!(by_ids.charge_ids("c", IDS_PER_TOKEN - 1));
        assert!(by_ids.charge_ids("c", 2 * IDS_PER_TOKEN));
        assert!(!by_ids.charge_ids("c", 2 * IDS_PER_TOKEN));
        assert!(by_ids.charge_ids("c", IDS_PER_TOKEN));
        assert!(by_ids.charge_ids("d", 1_000_000 * IDS_PER_TOKEN));
        assert!(!by_ids.check("d"));
        let svc = GovernorService::new(slow_service(Duration::ZERO), limits.clone());
        // Anonymous key (no peer info) shares one bucket.
        let r1 = svc.clone().oneshot(req()).await.unwrap();
        let r2 = svc.clone().oneshot(req()).await.unwrap();
        let r3 = svc.clone().oneshot(req()).await.unwrap();
        assert_eq!(code(&r1), None);
        assert_eq!(code(&r2), None);
        assert_eq!(code(&r3), Some(tonic::Code::ResourceExhausted));

        let unlimited = Limits::new(&RateLimitSection {
            per_principal_rps: 0,
            burst: 0,
            max_inflight: 1,
        });
        for _ in 0..100 {
            assert!(unlimited.check("a"));
        }
    }
}
