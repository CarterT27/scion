//! Request routing and verbatim forwarding (SPEC 3.3, 7.8; PLAN 6.13).
//!
//! Every path other than the locally served ObjectService methods is forwarded to the
//! upstream byte for byte at the HTTP/2 level: request headers (including `authorization`),
//! the length-prefixed gRPC message frames and the response headers and trailers pass through
//! unmodified, so forwarded responses keep the server's `scion-tier: server`. This is the
//! RawCodec passthrough taken one step lower: messages are not even re-framed. Epochs are
//! observed from the forwarded trailers (SPEC 10.4, 13.3).

use super::objects::LocalObjects;
use super::state::Inner;
use bytes::Bytes;
use futures::future::BoxFuture;
use http_body::{Body as HttpBody, Frame, SizeHint};
use pin_project_lite::pin_project;
use scion_core::{ScionError, subcode};
use scion_proto::{error_from_status, read_epochs, status_from_error, v1};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;
use tonic::body::Body;
use tower::ServiceExt;
use tower_service::Service;

/// `ObjectService/GetServerInfo`.
pub(crate) const GET_SERVER_INFO: &str = "/scion.v1.ObjectService/GetServerInfo";
/// `ObjectService/GetObjects`.
pub(crate) const GET_OBJECTS: &str = "/scion.v1.ObjectService/GetObjects";
/// `ObjectService/ResolveRef`.
pub(crate) const RESOLVE_REF: &str = "/scion.v1.ObjectService/ResolveRef";

/// The top-level service: local ObjectService methods or verbatim forwarding, selected by
/// gRPC path.
#[derive(Clone)]
pub(crate) struct Dispatch {
    inner: Arc<Inner>,
    local: v1::object_service_server::ObjectServiceServer<LocalObjects>,
}

impl Dispatch {
    pub fn new(inner: Arc<Inner>) -> Self {
        let local =
            v1::object_service_server::ObjectServiceServer::new(LocalObjects(inner.clone()))
                .max_decoding_message_size(64 * 1024 * 1024);
        Dispatch { inner, local }
    }

    /// True when `path` is answered by the daemon itself (SPEC 7.3 served by every tier;
    /// HasObjects is forwarded because it depends on upload records, PLAN 6.13). In
    /// fail-closed mode reads are forwarded too (SPEC 13.3).
    fn serves_locally(&self, path: &str) -> bool {
        match path {
            GET_SERVER_INFO => true,
            GET_OBJECTS | RESOLVE_REF => !self.inner.fail_closed_now(),
            _ => false,
        }
    }
}

impl Service<http::Request<Body>> for Dispatch {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Body>) -> Self::Future {
        if self.serves_locally(req.uri().path()) {
            let mut local = self.local.clone();
            Box::pin(async move { local.call(req).await })
        } else {
            let inner = self.inner.clone();
            Box::pin(async move { Ok(forward(inner, req).await) })
        }
    }
}

/// Last path segment of a gRPC path (the RPC name).
fn rpc_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

/// The SPEC 7.2 code name of a gRPC status found in `h` (`grpc-status` plus details).
fn status_name(h: &http::HeaderMap) -> Option<String> {
    let st = tonic::Status::from_header_map(h)?;
    if st.code() == tonic::Code::Ok {
        return Some("OK".to_owned());
    }
    Some(error_from_status(&st).code.as_str().to_owned())
}

/// What is logged once per forwarded call.
struct LogCtx {
    rpc: String,
    request_id: String,
    principal: Option<String>,
    started: Instant,
}

impl LogCtx {
    fn emit(self, inner: &Inner, status: &str) {
        inner.log_rpc(
            &self.rpc,
            &self.request_id,
            self.principal,
            status,
            self.started,
        );
    }
}

/// Forward one request upstream verbatim (SPEC 7.8).
pub(crate) async fn forward(inner: Arc<Inner>, req: http::Request<Body>) -> http::Response<Body> {
    let started = Instant::now();
    let rpc = rpc_name(req.uri().path());
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // Local verification only identifies the principal for logs and remembers a valid token
    // for redaction polls; authorization is the upstream's job.
    let principal = super::state::bearer(req.headers()).and_then(|t| inner.verify_cached(&t));
    if principal.is_some() && inner.fail_closed_now() {
        inner.poll_now.notify_one();
    }
    let log = LogCtx {
        rpc,
        request_id,
        principal,
        started,
    };
    let client = match inner.client().await {
        Ok(c) => c,
        Err(e) => {
            log.emit(&inner, e.code.as_str());
            return status_from_error(&e).into_http();
        }
    };
    inner.count_upstream();
    let mut ch = client.channel();
    let res = match ch.ready().await {
        Ok(ch) => ch.call(req).await,
        Err(e) => Err(e),
    };
    match res {
        Err(e) => {
            inner.reset_client().await;
            let err = ScionError::unavailable(
                subcode::UPSTREAM_UNAVAILABLE,
                format!("upstream request failed: {e}"),
            );
            log.emit(&inner, err.code.as_str());
            status_from_error(&err).into_http()
        }
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            if let Some(e) = read_epochs(&parts.headers) {
                inner.observe_epochs(e);
            }
            // Trailers-only response: the status is in the headers.
            let log = match status_name(&parts.headers) {
                Some(s) => {
                    log.emit(&inner, &s);
                    None
                }
                None => Some(log),
            };
            let body = ObserveBody {
                inner: body,
                daemon: inner,
                log,
            };
            http::Response::from_parts(parts, Body::new(body))
        }
    }
}

pin_project! {
    /// A forwarded response body: frames pass through unchanged; the trailers are inspected
    /// for epochs and the call status.
    struct ObserveBody {
        #[pin]
        inner: Body,
        daemon: Arc<Inner>,
        log: Option<LogCtx>,
    }
}

impl HttpBody for ObserveBody {
    type Data = Bytes;
    type Error = tonic::Status;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.project();
        let polled = this.inner.poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(t) = frame.trailers_ref() {
                    if let Some(e) = read_epochs(t) {
                        this.daemon.observe_epochs(e);
                    }
                    if let Some(log) = this.log.take() {
                        let s = status_name(t).unwrap_or_else(|| "OK".to_owned());
                        log.emit(this.daemon, &s);
                    }
                }
            }
            Poll::Ready(Some(Err(_))) => {
                if let Some(log) = this.log.take() {
                    log.emit(this.daemon, "UNAVAILABLE");
                }
            }
            Poll::Ready(None) => {
                if let Some(log) = this.log.take() {
                    log.emit(this.daemon, "OK");
                }
            }
            Poll::Pending => {}
        }
        polled
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

    #[test]
    fn rpc_names() {
        assert_eq!(rpc_name("/scion.v1.QueryService/Log"), "Log");
        assert_eq!(rpc_name("Log"), "Log");
    }

    #[test]
    fn status_names() {
        let mut h = http::HeaderMap::new();
        assert_eq!(status_name(&h), None);
        h.insert("grpc-status", "0".parse().unwrap());
        assert_eq!(status_name(&h).as_deref(), Some("OK"));
        let st = status_from_error(&ScionError::not_found("x"));
        let resp: http::Response<Body> = st.into_http();
        assert_eq!(status_name(resp.headers()).as_deref(), Some("NOT_FOUND"));
    }
}
