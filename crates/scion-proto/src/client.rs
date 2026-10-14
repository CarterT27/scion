//! [`ScionClient`]: a `scion.v1` client over a Unix socket or HTTPS (SPEC 7.1; P08).
//!
//! The transport stack of every generated client is
//! `InterceptedService<EpochCapture<Channel>, BearerInterceptor>`:
//! - [`BearerInterceptor`] adds `authorization: Bearer <token>` (SPEC 7.1);
//! - [`EpochCapture`] counts calls and records the epochs of the most recent response, read from
//!   the trailers frame or, for trailers-only responses, from the headers (SPEC 7.1).

use crate::endpoint::{Endpoint, TlsOptions};
use crate::trailers::{Epochs, read_epochs};
use crate::v1;
use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use pin_project_lite::pin_project;
use scion_core::{ErrorCode, Result, ScionError, subcode};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::Duration;
use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Certificate, Channel, ClientTlsConfig};

/// Largest response message the generated clients accept (tonic's default is 4 MiB; object
/// chunks and listings can be larger).
const MAX_DECODING_MESSAGE_BYTES: usize = 256 * 1024 * 1024;
/// Connection establishment timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// HTTP/2 header list size the client advertises. Error details (SPEC 7.2) travel in the
/// `grpc-status-details-bin` header (trailers-only responses put them in the headers), and a
/// MERGE_CONFLICT report can be far larger than hyper's 16 KiB default.
const MAX_HEADER_LIST_BYTES: u32 = 16 * 1024 * 1024;

/// Adds `authorization: Bearer <token>` to every request when a token is set (SPEC 7.1).
#[derive(Clone, Debug, Default)]
pub struct BearerInterceptor {
    token: Option<Arc<str>>,
}

/// A tower service wrapper around the channel that captures the epochs from response trailers
/// (and trailers-only headers) and counts calls.
#[derive(Clone, Debug)]
pub struct EpochCapture<S> {
    inner: S,
    state: Arc<ClientState>,
}

/// Shared per-client state: last observed epochs and the call counter.
#[derive(Debug, Default)]
pub(crate) struct ClientState {
    pub(crate) last_epochs: std::sync::Mutex<Option<Epochs>>,
    pub(crate) calls: std::sync::atomic::AtomicU64,
}

/// The transport stack used by every generated client of [`ScionClient`].
pub type Transport = InterceptedService<EpochCapture<Channel>, BearerInterceptor>;

/// A connected `scion.v1` client (SPEC 7.1). Cheap to clone.
#[derive(Clone)]
pub struct ScionClient {
    channel: Channel,
    token: Option<Arc<str>>,
    state: Arc<ClientState>,
}

impl BearerInterceptor {
    /// An interceptor sending `token`; `None` sends no `authorization` header (SPEC 7.1).
    pub fn new(token: Option<String>) -> Self {
        BearerInterceptor {
            token: token.map(Arc::from),
        }
    }
}

impl tonic::service::Interceptor for BearerInterceptor {
    fn call(
        &mut self,
        mut req: tonic::Request<()>,
    ) -> std::result::Result<tonic::Request<()>, tonic::Status> {
        if let Some(t) = &self.token {
            let v: MetadataValue<_> = format!("Bearer {t}")
                .parse()
                .map_err(|_| tonic::Status::unauthenticated("token is not a valid header value"))?;
            req.metadata_mut().insert("authorization", v);
        }
        Ok(req)
    }
}

impl ClientState {
    fn record(&self, e: Epochs) {
        let mut g = self
            .last_epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g = Some(e);
    }

    fn last(&self) -> Option<Epochs> {
        *self
            .last_epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<S> EpochCapture<S> {
    /// Wrap `inner`, recording into a fresh state (SPEC 7.1).
    pub fn new(inner: S) -> Self {
        EpochCapture {
            inner,
            state: Arc::new(ClientState::default()),
        }
    }

    /// Epochs captured from the most recent response (SPEC 7.1).
    pub fn last_epochs(&self) -> Option<Epochs> {
        self.state.last()
    }

    /// Calls issued through this wrapper and its clones.
    pub fn calls(&self) -> u64 {
        self.state.calls.load(Ordering::Relaxed)
    }
}

impl<S, ReqBody, ResBody> tower_service::Service<http::Request<ReqBody>> for EpochCapture<S>
where
    S: tower_service::Service<http::Request<ReqBody>, Response = http::Response<ResBody>>,
    ResBody: Body<Data = Bytes>,
{
    type Response = http::Response<CaptureBody<ResBody>>;
    type Error = S::Error;
    type Future = CaptureFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqBody>) -> Self::Future {
        self.state.calls.fetch_add(1, Ordering::Relaxed);
        CaptureFuture {
            inner: self.inner.call(req),
            state: self.state.clone(),
        }
    }
}

pin_project! {
    /// Response future of [`EpochCapture`]: records epochs found in trailers-only headers.
    pub struct CaptureFuture<F> {
        #[pin]
        inner: F,
        state: Arc<ClientState>,
    }
}

impl<F, ResBody, E> Future for CaptureFuture<F>
where
    F: Future<Output = std::result::Result<http::Response<ResBody>, E>>,
{
    type Output = std::result::Result<http::Response<CaptureBody<ResBody>>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let res = match this.inner.poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(r) => r,
        };
        Poll::Ready(res.map(|resp| {
            if let Some(e) = read_epochs(resp.headers()) {
                this.state.record(e);
            }
            resp.map(|inner| CaptureBody {
                inner,
                state: this.state.clone(),
            })
        }))
    }
}

pin_project! {
    /// Response body of [`EpochCapture`]: records epochs found in the trailers frame.
    pub struct CaptureBody<B> {
        #[pin]
        inner: B,
        state: Arc<ClientState>,
    }
}

impl<B> Body for CaptureBody<B>
where
    B: Body<Data = Bytes>,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, Self::Error>>> {
        let this = self.project();
        let polled = this.inner.poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &polled
            && let Some(t) = frame.trailers_ref()
            && let Some(e) = read_epochs(t)
        {
            this.state.record(e);
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

fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        out.push_str(": ");
        out.push_str(&s.to_string());
        cur = s.source();
    }
    out
}

fn unavailable(ep: &Endpoint, e: &(dyn std::error::Error + 'static)) -> ScionError {
    ScionError::unavailable(
        subcode::UPSTREAM_UNAVAILABLE,
        format!("cannot connect to {ep}: {}", error_chain(e)),
    )
}

impl ScionClient {
    /// Connect to `ep` (Unix socket or HTTPS with rustls), optionally authenticating with
    /// `token` (SPEC 7.1, 17).
    ///
    /// HTTPS trusts `tls.ca_pem` when given, otherwise the native root store; `tls.domain`
    /// overrides the name verified against the certificate. TLS options are ignored for Unix
    /// sockets. Failure to connect is UNAVAILABLE/upstream_unavailable.
    pub async fn connect(
        ep: &Endpoint,
        tls: Option<TlsOptions>,
        token: Option<String>,
    ) -> Result<Self> {
        let channel = match ep {
            Endpoint::Unix(path) => {
                let path = path.clone();
                let connector = tower::service_fn(move |_: http::Uri| {
                    let path = path.clone();
                    async move {
                        let s = tokio::net::UnixStream::connect(path).await?;
                        Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(s))
                    }
                });
                tonic::transport::Endpoint::from_static("http://localhost")
                    .connect_timeout(CONNECT_TIMEOUT)
                    .http2_max_header_list_size(MAX_HEADER_LIST_BYTES)
                    .connect_with_connector(connector)
                    .await
                    .map_err(|e| unavailable(ep, &e))?
            }
            Endpoint::Https(url) => {
                let tls = tls.unwrap_or_default();
                let mut cfg = ClientTlsConfig::new();
                cfg = match tls.ca_pem {
                    Some(pem) => cfg.ca_certificate(Certificate::from_pem(pem)),
                    None => cfg.with_native_roots(),
                };
                if let Some(d) = tls.domain {
                    cfg = cfg.domain_name(d);
                }
                let endpoint = tonic::transport::Endpoint::from_shared(url.clone())
                    .map_err(|e| {
                        ScionError::new(
                            ErrorCode::InvalidArgument,
                            format!("invalid endpoint {url}: {}", error_chain(&e)),
                        )
                    })?
                    .connect_timeout(CONNECT_TIMEOUT)
                    .http2_max_header_list_size(MAX_HEADER_LIST_BYTES)
                    .tls_config(cfg)
                    .map_err(|e| unavailable(ep, &e))?;
                endpoint.connect().await.map_err(|e| unavailable(ep, &e))?
            }
        };
        Ok(ScionClient {
            channel,
            token: token.map(Arc::from),
            state: Arc::new(ClientState::default()),
        })
    }

    /// The same connection with a different bearer token. Epoch capture and the call counter
    /// are shared with `self`.
    pub fn with_token(&self, token: Option<String>) -> Self {
        ScionClient {
            channel: self.channel.clone(),
            token: token.map(Arc::from),
            state: self.state.clone(),
        }
    }

    fn transport(&self) -> Transport {
        InterceptedService::new(
            EpochCapture {
                inner: self.channel.clone(),
                state: self.state.clone(),
            },
            BearerInterceptor {
                token: self.token.clone(),
            },
        )
    }

    /// ObjectService client (SPEC 7.3).
    pub fn objects(&self) -> v1::object_service_client::ObjectServiceClient<Transport> {
        v1::object_service_client::ObjectServiceClient::new(self.transport())
            .max_decoding_message_size(MAX_DECODING_MESSAGE_BYTES)
    }

    /// QueryService client (SPEC 7.4).
    pub fn query(&self) -> v1::query_service_client::QueryServiceClient<Transport> {
        v1::query_service_client::QueryServiceClient::new(self.transport())
            .max_decoding_message_size(MAX_DECODING_MESSAGE_BYTES)
    }

    /// WriteService client (SPEC 7.5).
    pub fn write(&self) -> v1::write_service_client::WriteServiceClient<Transport> {
        v1::write_service_client::WriteServiceClient::new(self.transport())
            .max_decoding_message_size(MAX_DECODING_MESSAGE_BYTES)
    }

    /// RefService client (SPEC 7.6).
    pub fn refs(&self) -> v1::ref_service_client::RefServiceClient<Transport> {
        v1::ref_service_client::RefServiceClient::new(self.transport())
            .max_decoding_message_size(MAX_DECODING_MESSAGE_BYTES)
    }

    /// AuthService client (SPEC 7.7).
    pub fn auth(&self) -> v1::auth_service_client::AuthServiceClient<Transport> {
        v1::auth_service_client::AuthServiceClient::new(self.transport())
            .max_decoding_message_size(MAX_DECODING_MESSAGE_BYTES)
    }

    /// AdminService client (SPEC 7.7).
    pub fn admin(&self) -> v1::admin_service_client::AdminServiceClient<Transport> {
        v1::admin_service_client::AdminServiceClient::new(self.transport())
            .max_decoding_message_size(MAX_DECODING_MESSAGE_BYTES)
    }

    /// Epochs captured from the most recent response trailers (SPEC 7.1).
    pub fn last_epochs(&self) -> Option<Epochs> {
        self.state.last()
    }

    /// Number of RPCs issued through this client (and its clones).
    pub fn calls(&self) -> u64 {
        self.state.calls.load(Ordering::Relaxed)
    }

    /// Reset the call counter.
    pub fn reset_calls(&self) {
        self.state.calls.store(0, Ordering::Relaxed);
    }

    /// The underlying channel.
    pub fn channel(&self) -> Channel {
        self.channel.clone()
    }

    /// The bearer token this client sends, if any (SPEC 7.1).
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }
}

impl std::fmt::Debug for ScionClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the token (PLAN 6.9).
        f.debug_struct("ScionClient")
            .field("authenticated", &self.token.is_some())
            .field("calls", &self.calls())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::service::Interceptor;

    #[test]
    fn bearer_interceptor() {
        let mut i = BearerInterceptor::new(Some("tok".into()));
        let r = i.call(tonic::Request::new(())).unwrap();
        assert_eq!(r.metadata().get("authorization").unwrap(), "Bearer tok");
        let mut i = BearerInterceptor::new(None);
        let r = i.call(tonic::Request::new(())).unwrap();
        assert!(r.metadata().get("authorization").is_none());
        let mut i = BearerInterceptor::new(Some("bad\ntoken".into()));
        assert_eq!(
            i.call(tonic::Request::new(())).unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
    }

    #[tokio::test]
    async fn connect_failure_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let ep = Endpoint::Unix(dir.path().join("missing.sock"));
        let e = ScionClient::connect(&ep, None, None).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Unavailable);
        assert_eq!(e.subcode(), Some(subcode::UPSTREAM_UNAVAILABLE));
    }
}
