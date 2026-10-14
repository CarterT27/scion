//! SPEC 7.1 epoch trailers on every response (P08).
//!
//! [`EpochTrailerLayer`] wraps a gRPC HTTP service. For a normal response it appends the three
//! keys to the HTTP/2 trailers frame (creating one if the inner body ends without trailers).
//! For a trailers-only response (tonic puts `grpc-status` in the headers and sends no body) it
//! adds the keys to those headers. Values are read from the [`EpochSource`] when the trailers
//! are produced, so they reflect the state at the end of the call.

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue};
use http_body::{Body, Frame, SizeHint};
use pin_project_lite::pin_project;
use scion_core::Tier;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// Trailer carrying the policy epoch as a decimal u64 (SPEC 7.1, PLAN 6.2).
pub const TRAILER_POLICY_EPOCH: &str = "scion-policy-epoch";
/// Trailer carrying the redaction epoch as a decimal u64 (SPEC 7.1, PLAN 6.2).
pub const TRAILER_REDACTION_EPOCH: &str = "scion-redaction-epoch";
/// Trailer carrying the serving tier, `server` or `daemon` (SPEC 7.1).
pub const TRAILER_TIER: &str = "scion-tier";

/// The `grpc-status` key; its presence in response headers marks a trailers-only response.
const GRPC_STATUS: &str = "grpc-status";

/// The three trailer values (SPEC 7.1).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Epochs {
    /// `scion-policy-epoch`.
    pub policy: u64,
    /// `scion-redaction-epoch`.
    pub redaction: u64,
    /// `scion-tier`.
    pub tier: Tier,
}

/// Supplies the current epochs for [`EpochTrailerLayer`].
pub trait EpochSource: Send + Sync + 'static {
    /// The epochs to stamp on the response being completed.
    fn epochs(&self) -> Epochs;
}

/// A fixed [`EpochSource`] (SPEC 7.1), convenient for tests and static tiers.
impl EpochSource for Epochs {
    fn epochs(&self) -> Epochs {
        *self
    }
}

fn parse_u64(h: &HeaderMap, key: &str) -> Option<u64> {
    let s = h.get(key)?.to_str().ok()?;
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Parse the three epoch keys from a header or trailer map; `None` unless all are valid.
pub fn read_epochs(h: &HeaderMap) -> Option<Epochs> {
    let policy = parse_u64(h, TRAILER_POLICY_EPOCH)?;
    let redaction = parse_u64(h, TRAILER_REDACTION_EPOCH)?;
    let tier = Tier::parse(h.get(TRAILER_TIER)?.to_str().ok()?)?;
    Some(Epochs {
        policy,
        redaction,
        tier,
    })
}

/// Write the three epoch keys into a header or trailer map.
pub fn write_epochs(h: &mut HeaderMap, e: &Epochs) {
    h.insert(
        HeaderName::from_static(TRAILER_POLICY_EPOCH),
        HeaderValue::from(e.policy),
    );
    h.insert(
        HeaderName::from_static(TRAILER_REDACTION_EPOCH),
        HeaderValue::from(e.redaction),
    );
    h.insert(
        HeaderName::from_static(TRAILER_TIER),
        HeaderValue::from_static(e.tier.as_str()),
    );
}

/// A tower `Layer` over HTTP bodies that appends the three trailers to the gRPC trailers frame of
/// EVERY response: unary, streaming, error and trailers-only (SPEC 7.1).
#[derive(Clone)]
pub struct EpochTrailerLayer {
    src: Arc<dyn EpochSource>,
}

impl EpochTrailerLayer {
    /// A layer stamping epochs from `src`.
    pub fn new(src: Arc<dyn EpochSource>) -> Self {
        EpochTrailerLayer { src }
    }
}

impl std::fmt::Debug for EpochTrailerLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochTrailerLayer").finish_non_exhaustive()
    }
}

impl<S> tower_layer::Layer<S> for EpochTrailerLayer {
    type Service = EpochTrailerService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        EpochTrailerService {
            inner,
            src: self.src.clone(),
        }
    }
}

/// The service produced by [`EpochTrailerLayer`] (SPEC 7.1).
#[derive(Clone)]
pub struct EpochTrailerService<S> {
    inner: S,
    src: Arc<dyn EpochSource>,
}

impl<S: std::fmt::Debug> std::fmt::Debug for EpochTrailerService<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochTrailerService")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<S, ReqBody, ResBody> tower_service::Service<http::Request<ReqBody>> for EpochTrailerService<S>
where
    S: tower_service::Service<http::Request<ReqBody>, Response = http::Response<ResBody>>,
    ResBody: Body<Data = Bytes>,
{
    type Response = http::Response<EpochBody<ResBody>>;
    type Error = S::Error;
    type Future = EpochFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqBody>) -> Self::Future {
        EpochFuture {
            inner: self.inner.call(req),
            src: self.src.clone(),
        }
    }
}

pin_project! {
    /// Response future of [`EpochTrailerService`].
    pub struct EpochFuture<F> {
        #[pin]
        inner: F,
        src: Arc<dyn EpochSource>,
    }
}

impl<F, ResBody, E> Future for EpochFuture<F>
where
    F: Future<Output = Result<http::Response<ResBody>, E>>,
    ResBody: Body<Data = Bytes>,
{
    type Output = Result<http::Response<EpochBody<ResBody>>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let res = match this.inner.poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(r) => r,
        };
        let src = this.src.clone();
        Poll::Ready(res.map(|resp| {
            let (mut parts, body) = resp.into_parts();
            let trailers_only = parts.headers.contains_key(GRPC_STATUS);
            if trailers_only {
                write_epochs(&mut parts.headers, &src.epochs());
            }
            let body = EpochBody {
                inner: body,
                src,
                // A trailers-only response already carries the keys; a body that still yields a
                // trailers frame gets them added there too.
                state: if trailers_only {
                    BodyState::HeadersStamped
                } else {
                    BodyState::Streaming
                },
            };
            http::Response::from_parts(parts, body)
        }))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum BodyState {
    /// Normal response: trailers not yet seen.
    Streaming,
    /// Trailers-only response: keys are in the headers; no synthesized trailers frame.
    HeadersStamped,
    /// The epoch trailers were emitted (or the body failed); nothing more to add.
    Done,
}

pin_project! {
    /// Response body of [`EpochTrailerService`]: adds the epoch keys to the trailers frame, or
    /// emits a trailers frame carrying only them when the inner body ends without one.
    pub struct EpochBody<B> {
        #[pin]
        inner: B,
        src: Arc<dyn EpochSource>,
        state: BodyState,
    }
}

impl<B> Body for EpochBody<B>
where
    B: Body<Data = Bytes>,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        if *this.state == BodyState::Done {
            return Poll::Ready(None);
        }
        match this.inner.poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(frame))) => match frame.into_trailers() {
                Ok(mut trailers) => {
                    write_epochs(&mut trailers, &this.src.epochs());
                    *this.state = BodyState::Done;
                    Poll::Ready(Some(Ok(Frame::trailers(trailers))))
                }
                Err(frame) => Poll::Ready(Some(Ok(frame))),
            },
            Poll::Ready(Some(Err(e))) => {
                *this.state = BodyState::Done;
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                let state = *this.state;
                *this.state = BodyState::Done;
                if state == BodyState::Streaming {
                    let mut trailers = HeaderMap::new();
                    write_epochs(&mut trailers, &this.src.epochs());
                    Poll::Ready(Some(Ok(Frame::trailers(trailers))))
                } else {
                    Poll::Ready(None)
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        match self.state {
            BodyState::Done => true,
            BodyState::HeadersStamped => self.inner.is_end_stream(),
            BodyState::Streaming => false,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self.state {
            BodyState::Done => SizeHint::with_exact(0),
            _ => {
                // Trailers carry no data bytes, but an exact hint of 0 could let a transport end
                // the stream early, so only report the lower bound.
                let h = self.inner.size_hint();
                let mut out = SizeHint::new();
                out.set_lower(h.lower());
                if let Some(u) = h.upper()
                    && self.state == BodyState::HeadersStamped
                {
                    out.set_upper(u);
                }
                out
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Empty, Full, StreamBody};
    use std::convert::Infallible;
    use tower::{ServiceExt, service_fn};

    fn src(p: u64, r: u64) -> Arc<dyn EpochSource> {
        Arc::new(Epochs {
            policy: p,
            redaction: r,
            tier: Tier::Server,
        })
    }

    #[test]
    fn read_write_round_trip() {
        let e = Epochs {
            policy: u64::MAX,
            redaction: 0,
            tier: Tier::Daemon,
        };
        let mut h = HeaderMap::new();
        write_epochs(&mut h, &e);
        assert_eq!(h.get(TRAILER_POLICY_EPOCH).unwrap(), "18446744073709551615");
        assert_eq!(h.get(TRAILER_REDACTION_EPOCH).unwrap(), "0");
        assert_eq!(h.get(TRAILER_TIER).unwrap(), "daemon");
        assert_eq!(read_epochs(&h), Some(e));
        h.insert(TRAILER_POLICY_EPOCH, HeaderValue::from_static("+1"));
        assert_eq!(read_epochs(&h), None);
        h.insert(TRAILER_POLICY_EPOCH, HeaderValue::from_static("1"));
        h.insert(TRAILER_TIER, HeaderValue::from_static("cache"));
        assert_eq!(read_epochs(&h), None);
        h.remove(TRAILER_TIER);
        assert_eq!(read_epochs(&h), None);
    }

    async fn collect_trailers<B>(b: B) -> (Bytes, Option<HeaderMap>)
    where
        B: Body<Data = Bytes>,
        B::Error: std::fmt::Debug,
    {
        let c = b.collect().await.unwrap();
        let t = c.trailers().cloned();
        (c.to_bytes(), t)
    }

    #[tokio::test]
    async fn appends_to_existing_trailers() {
        let svc = service_fn(|_req: http::Request<()>| async {
            let frames: Vec<Result<Frame<Bytes>, Infallible>> = vec![
                Ok(Frame::data(Bytes::from_static(b"abc"))),
                Ok(Frame::trailers({
                    let mut h = HeaderMap::new();
                    h.insert("grpc-status", HeaderValue::from_static("0"));
                    h
                })),
            ];
            Ok::<_, Infallible>(http::Response::new(StreamBody::new(futures::stream::iter(
                frames,
            ))))
        });
        let svc = tower_layer::Layer::layer(&EpochTrailerLayer::new(src(5, 6)), svc);
        let resp = svc.oneshot(http::Request::new(())).await.unwrap();
        assert!(read_epochs(resp.headers()).is_none());
        let (data, trailers) = collect_trailers(resp.into_body()).await;
        assert_eq!(&data[..], b"abc");
        let t = trailers.unwrap();
        assert_eq!(t.get("grpc-status").unwrap(), "0");
        assert_eq!(
            read_epochs(&t),
            Some(Epochs {
                policy: 5,
                redaction: 6,
                tier: Tier::Server
            })
        );
    }

    #[tokio::test]
    async fn synthesizes_trailers_when_missing() {
        let svc = service_fn(|_req: http::Request<()>| async {
            Ok::<_, Infallible>(http::Response::new(Full::new(Bytes::from_static(b"x"))))
        });
        let svc = tower_layer::Layer::layer(&EpochTrailerLayer::new(src(1, 2)), svc);
        let resp = svc.oneshot(http::Request::new(())).await.unwrap();
        assert!(!resp.body().is_end_stream());
        let (data, trailers) = collect_trailers(resp.into_body()).await;
        assert_eq!(&data[..], b"x");
        assert_eq!(read_epochs(&trailers.unwrap()).unwrap().redaction, 2);
    }

    #[tokio::test]
    async fn trailers_only_stamps_headers() {
        let svc = service_fn(|_req: http::Request<()>| async {
            let mut r = http::Response::new(Empty::<Bytes>::new());
            r.headers_mut()
                .insert("grpc-status", HeaderValue::from_static("5"));
            Ok::<_, Infallible>(r)
        });
        let svc = tower_layer::Layer::layer(&EpochTrailerLayer::new(src(9, 10)), svc);
        let resp = svc.oneshot(http::Request::new(())).await.unwrap();
        assert_eq!(read_epochs(resp.headers()).unwrap().policy, 9);
        assert!(resp.body().is_end_stream());
        let (data, trailers) = collect_trailers(resp.into_body()).await;
        assert!(data.is_empty());
        assert!(trailers.is_none());
    }
}
