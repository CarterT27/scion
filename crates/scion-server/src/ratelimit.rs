//! Load shedding and per-principal rate limits (SPEC 10.5; PLAN 6.11 `[rate_limit]`; P23).
//!
//! - [`ShedService`]: a tower `ConcurrencyLimit` over one semaphore shared by every service of
//!   the server (`rate_limit.max_inflight` permits) behind `LoadShed`, so a call that finds no
//!   free permit fails RESOURCE_EXHAUSTED immediately instead of queueing (the server sheds
//!   load before its dependencies saturate).
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
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use tokio::sync::Semaphore;
use tower::ServiceExt;
use tower::limit::ConcurrencyLimit;
use tower::load_shed::LoadShed;
use tower_service::Service;

/// How many limiter checks happen between purges of idle per-principal state.
const RETAIN_EVERY: u64 = 4096;

/// Shared limits of one server: the in-flight semaphore and the per-principal limiter.
pub struct Limits {
    inflight: Arc<Semaphore>,
    limiter: Option<DefaultKeyedRateLimiter<String>>,
    checks: AtomicU64,
}

impl Limits {
    /// Limits from `[rate_limit]` (SPEC 10.5, PLAN 6.11). A zero `per_principal_rps` disables
    /// the per-principal limiter; a zero burst is treated as 1.
    pub fn new(cfg: &scion_config::RateLimitSection) -> Arc<Self> {
        let limiter = NonZeroU32::new(cfg.per_principal_rps).map(|rps| {
            let burst = NonZeroU32::new(cfg.burst).unwrap_or(NonZeroU32::MIN);
            RateLimiter::keyed(Quota::per_second(rps).allow_burst(burst))
        });
        Arc::new(Limits {
            inflight: Arc::new(Semaphore::new(cfg.max_inflight.min(Semaphore::MAX_PERMITS))),
            limiter,
            checks: AtomicU64::new(0),
        })
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

/// Load shedding: `LoadShed<ConcurrencyLimit>` over the shared in-flight semaphore; an
/// overloaded call fails RESOURCE_EXHAUSTED (SPEC 10.5).
#[derive(Clone)]
pub struct ShedService {
    inner: LoadShed<ConcurrencyLimit<BoxSvc>>,
}

impl ShedService {
    /// Wrap `inner` with the shared in-flight limit of `limits`.
    pub fn new(inner: BoxSvc, limits: &Limits) -> Self {
        ShedService {
            inner: LoadShed::new(ConcurrencyLimit::with_semaphore(
                inner,
                limits.inflight.clone(),
            )),
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
        let svc = self.inner.clone();
        Box::pin(async move {
            match svc.oneshot(req).await {
                Ok(resp) => Ok(resp),
                Err(e) => {
                    let err = if e.is::<tower::load_shed::error::Overloaded>() {
                        ScionError::resource_exhausted("server overloaded; retry later")
                    } else {
                        ScionError::unavailable(
                            scion_core::subcode::STORE_UNAVAILABLE,
                            format!("request failed: {e}"),
                        )
                    };
                    Ok(error_response(&err))
                }
            }
        })
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
        assert_eq!(limits.available_permits(), 2);
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
