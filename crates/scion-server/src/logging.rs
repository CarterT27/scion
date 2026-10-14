//! RPC log events and RPC metrics (SPEC 15.1, 15.2; PLAN 6.9; P23).
//!
//! [`LogService`] wraps every gRPC service. When the response completes (the trailers frame of
//! a normal response, the headers of a trailers-only response, or the body being dropped by a
//! cancelled call) it emits exactly one [`RpcLogEvent`] with tier `server` and observes
//! `scion_rpc_duration_seconds{rpc,tier,code}`. Inner layers and handlers add fields through
//! the [`LogSlot`] in the request extensions (the interceptor sets `principal`; handlers may set
//! `changeset_id`, `ref`, `ref_version`, `land_attempt` with [`annotate`]). Paths, tokens and
//! contents are never logged (PLAN 6.9).
#![allow(dead_code)] // `annotate` is offered to handlers owned by other packages.

use crate::errors::status_label;
use crate::interceptor::{BoxFut, BoxSvc, HttpReq, HttpResp, RequestId, rpc_name};
use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use parking_lot::Mutex;
use scion_obs::{Metrics, RpcLogEvent};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;
use tower::ServiceExt;
use tower_service::Service;

/// Optional SPEC 15.1 fields filled in while a call runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogFields {
    /// Authenticated (or, for `Login`, newly logged-in) principal.
    pub principal: Option<String>,
    /// Changeset id text, when applicable.
    pub changeset_id: Option<String>,
    /// Ref name, when applicable (logged as `ref`).
    pub ref_name: Option<String>,
    /// Ref version, when applicable.
    pub ref_version: Option<u64>,
    /// Land attempt number, when applicable.
    pub land_attempt: Option<u32>,
}

/// Per-call log fields shared between [`LogService`] and the layers/handlers inside it.
#[derive(Debug, Default)]
pub struct LogSlot(Mutex<LogFields>);

impl LogSlot {
    /// Modify the fields.
    pub fn update(&self, f: impl FnOnce(&mut LogFields)) {
        f(&mut self.0.lock());
    }

    /// A copy of the fields.
    pub fn get(&self) -> LogFields {
        self.0.lock().clone()
    }
}

/// Add SPEC 15.1 fields to the log event of the call `req` belongs to (no-op outside the
/// server stack).
pub fn annotate<T>(req: &tonic::Request<T>, f: impl FnOnce(&mut LogFields)) {
    if let Some(slot) = req.extensions().get::<Arc<LogSlot>>() {
        slot.update(f);
    }
}

/// Emits one RPC log event and duration observation per call (SPEC 15.1, 15.2).
#[derive(Clone)]
pub struct LogService {
    inner: BoxSvc,
    metrics: Arc<Metrics>,
    repo: Arc<str>,
}

impl LogService {
    /// Wrap `inner`; events carry `repo`.
    pub fn new(inner: BoxSvc, metrics: Arc<Metrics>, repo: &str) -> Self {
        LogService {
            inner,
            metrics,
            repo: Arc::from(repo),
        }
    }
}

impl Service<HttpReq> for LogService {
    type Response = HttpResp;
    type Error = Infallible;
    type Future = BoxFut;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: HttpReq) -> BoxFut {
        let start = Instant::now();
        let slot = Arc::new(LogSlot::default());
        req.extensions_mut().insert(slot.clone());
        let mut guard = EventGuard {
            metrics: self.metrics.clone(),
            repo: self.repo.clone(),
            rpc: rpc_name(req.uri().path()).to_owned(),
            request_id: req
                .extensions()
                .get::<RequestId>()
                .map(|r| r.0.clone())
                .unwrap_or_default(),
            slot,
            start,
            status: None,
            emitted: false,
        };
        let inner = self.inner.clone();
        Box::pin(async move {
            let resp = inner.oneshot(req).await?;
            // A trailers-only response carries its status in the headers.
            if let Some(s) = tonic::Status::from_header_map(resp.headers()) {
                guard.status = Some(status_label(Some(&s)));
            }
            let (parts, body) = resp.into_parts();
            let body = LogBody { inner: body, guard };
            Ok(http::Response::from_parts(
                parts,
                tonic::body::Body::new(body),
            ))
        })
    }
}

/// Holds what the event needs; emits on `finish` or, at the latest, on drop.
struct EventGuard {
    metrics: Arc<Metrics>,
    repo: Arc<str>,
    rpc: String,
    request_id: String,
    slot: Arc<LogSlot>,
    start: Instant,
    status: Option<String>,
    emitted: bool,
}

impl EventGuard {
    fn finish(&mut self, fallback: &str) {
        if self.emitted {
            return;
        }
        self.emitted = true;
        let elapsed = self.start.elapsed();
        let status = self.status.clone().unwrap_or_else(|| fallback.to_owned());
        self.metrics
            .rpc_duration
            .with_label_values(&[self.rpc.as_str(), "server", status.as_str()])
            .observe(elapsed.as_secs_f64());
        let f = self.slot.get();
        RpcLogEvent {
            request_id: self.request_id.clone(),
            rpc: self.rpc.clone(),
            tier: "server",
            principal: f.principal,
            repo: self.repo.to_string(),
            status,
            duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            changeset_id: f.changeset_id,
            ref_name: f.ref_name,
            ref_version: f.ref_version,
            land_attempt: f.land_attempt,
        }
        .emit();
    }
}

impl Drop for EventGuard {
    fn drop(&mut self) {
        // A body dropped before its trailers: the client went away.
        self.finish("CANCELLED");
    }
}

/// Response body that records the final status from the trailers frame.
struct LogBody {
    inner: tonic::body::Body,
    guard: EventGuard,
}

impl Body for LogBody {
    type Data = Bytes;
    type Error = tonic::Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, tonic::Status>>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(t) = frame.trailers_ref() {
                    let label = match tonic::Status::from_header_map(t) {
                        Some(s) => status_label(Some(&s)),
                        None => "OK".to_owned(),
                    };
                    this.guard.status = Some(label);
                    this.guard.finish("OK");
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(s))) => {
                if this.guard.status.is_none() {
                    this.guard.status = Some(status_label(Some(&s)));
                }
                this.guard.finish("UNKNOWN");
                Poll::Ready(Some(Err(s)))
            }
            Poll::Ready(None) => {
                this.guard.finish("OK");
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tower::util::BoxCloneSyncService;

    #[tokio::test]
    async fn observes_status_from_headers_and_trailers() {
        let metrics = Metrics::new();
        let failing = BoxCloneSyncService::new(tower::service_fn(|req: HttpReq| async move {
            annotate(&tonic::Request::from_http(req), |f| {
                f.ref_name = Some("main".into())
            });
            Ok::<_, Infallible>(tonic::Status::not_found("nope").into_http())
        }));
        let svc = LogService::new(failing, metrics.clone(), "r");
        let req = http::Request::builder()
            .uri("http://x/scion.v1.ObjectService/ResolveRef")
            .body(tonic::body::Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        drop(resp.into_body().collect().await);
        assert_eq!(
            metrics
                .rpc_duration
                .with_label_values(&["ResolveRef", "server", "NOT_FOUND"])
                .get_sample_count(),
            1
        );

        let streaming = BoxCloneSyncService::new(tower::service_fn(|_req: HttpReq| async move {
            let frames: Vec<Result<Frame<Bytes>, tonic::Status>> = vec![
                Ok(Frame::data(Bytes::from_static(b"x"))),
                Ok(Frame::trailers({
                    let mut h = http::HeaderMap::new();
                    h.insert("grpc-status", "0".parse().unwrap());
                    h
                })),
            ];
            Ok::<_, Infallible>(HttpResp::new(tonic::body::Body::new(
                http_body_util::StreamBody::new(futures::stream::iter(frames)),
            )))
        }));
        let svc = LogService::new(streaming, metrics.clone(), "r");
        let req = http::Request::builder()
            .uri("http://x/scion.v1.QueryService/Diff")
            .body(tonic::body::Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        let collected = resp.into_body().collect().await.unwrap();
        assert!(collected.trailers().is_some());
        assert_eq!(
            metrics
                .rpc_duration
                .with_label_values(&["Diff", "server", "OK"])
                .get_sample_count(),
            1
        );
    }
}
