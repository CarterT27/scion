//! Request id and bearer-token interceptor layers (SPEC 7.1, 12.1, 15.1; P23).
//!
//! Every gRPC service is wrapped in the same stack (outermost first; see `app.rs`):
//! request id, epoch trailers, RPC log/metrics, load shedding, the bearer interceptor, the
//! per-principal rate limiter, then the service. The interceptor verifies
//! `authorization: Bearer <token>` with the server clock for every RPC except `GetServerInfo`
//! and `Login`, inserts a [`CallerCtx`] into the request extensions, and rejects anything else
//! with UNAUTHENTICATED before the handler runs.

use crate::context::CallerCtx;
use crate::errors::error_response;
use crate::logging::LogSlot;
use scion_auth::Verifier;
use scion_core::{Clock, ErrorCode, ScionError};
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tower::ServiceExt;
use tower::util::BoxCloneSyncService;
use tower_service::Service;

/// The request type every layer handles.
pub type HttpReq = http::Request<tonic::body::Body>;
/// The response type every layer produces.
pub type HttpResp = http::Response<tonic::body::Body>;
/// A type-erased layer or service.
pub type BoxSvc = BoxCloneSyncService<HttpReq, HttpResp, Infallible>;
/// The response future of every layer.
pub type BoxFut = Pin<Box<dyn Future<Output = Result<HttpResp, Infallible>> + Send>>;

/// The full gRPC paths that need no token (SPEC 7.1).
pub const UNAUTHENTICATED_PATHS: [&str; 2] = [
    "/scion.v1.ObjectService/GetServerInfo",
    "/scion.v1.AuthService/Login",
];

/// The request-id header read from clients and echoed on responses (SPEC 15.1 `request_id`).
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The request id of a call, inserted into request extensions by [`RequestIdService`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestId(pub String);

/// The method name of a gRPC path (`/scion.v1.AuthService/WhoAmI` -> `WhoAmI`), used as the
/// `rpc` log field and metric label (SPEC 15.1, 15.2).
pub fn rpc_name(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((svc, m)) if !svc.is_empty() && !m.is_empty() => m,
        _ => path,
    }
}

/// Whether `path` requires a bearer token (SPEC 7.1).
pub fn requires_token(path: &str) -> bool {
    !UNAUTHENTICATED_PATHS.contains(&path)
}

fn acceptable_request_id(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 128
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// Assigns a request id: a well-formed client `x-request-id`, else a fresh UUID v4. Inserts
/// [`RequestId`] into the request extensions and echoes the id on the response (SPEC 15.1).
#[derive(Clone)]
pub struct RequestIdService {
    inner: BoxSvc,
}

impl RequestIdService {
    /// Wrap `inner`.
    pub fn new(inner: BoxSvc) -> Self {
        RequestIdService { inner }
    }
}

impl Service<HttpReq> for RequestIdService {
    type Response = HttpResp;
    type Error = Infallible;
    type Future = BoxFut;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: HttpReq) -> BoxFut {
        let id = req
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .filter(|v| acceptable_request_id(v))
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        req.extensions_mut().insert(RequestId(id.clone()));
        let inner = self.inner.clone();
        Box::pin(async move {
            let mut resp = inner.oneshot(req).await?;
            if let Ok(v) = http::HeaderValue::from_str(&id) {
                resp.headers_mut().insert(REQUEST_ID_HEADER, v);
            }
            Ok(resp)
        })
    }
}

/// The bearer token of `headers`, if any (`authorization: Bearer <token>`, scheme
/// case-insensitive; SPEC 7.1).
pub fn bearer_token(headers: &http::HeaderMap) -> Option<&str> {
    let v = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Verify the bearer token of a request at `now`: UNAUTHENTICATED for a missing, malformed,
/// expired, unknown-key or tampered token (SPEC 7.1, 12.1, 19.11).
pub fn authenticate(
    headers: &http::HeaderMap,
    verifier: &Verifier,
    clock: &dyn Clock,
) -> Result<scion_auth::Claims, ScionError> {
    let token =
        bearer_token(headers).ok_or_else(|| ScionError::unauthenticated("missing bearer token"))?;
    verifier.verify(token, clock.now()).map_err(|e| {
        if e.code == ErrorCode::Unauthenticated {
            e
        } else {
            ScionError::unauthenticated(e.message)
        }
    })
}

/// The bearer interceptor (SPEC 7.1, 12.1): every RPC except `GetServerInfo` and `Login`
/// needs a valid token; on success a [`CallerCtx`] is inserted into the request extensions.
#[derive(Clone)]
pub struct AuthService {
    inner: BoxSvc,
    verifier: Verifier,
    clock: Arc<dyn Clock>,
}

impl AuthService {
    /// Wrap `inner`, verifying with `verifier` at `clock.now()`.
    pub fn new(inner: BoxSvc, verifier: Verifier, clock: Arc<dyn Clock>) -> Self {
        AuthService {
            inner,
            verifier,
            clock,
        }
    }
}

impl Service<HttpReq> for AuthService {
    type Response = HttpResp;
    type Error = Infallible;
    type Future = BoxFut;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: HttpReq) -> BoxFut {
        let inner = self.inner.clone();
        if !requires_token(req.uri().path()) {
            return Box::pin(inner.oneshot(req));
        }
        match authenticate(req.headers(), &self.verifier, &*self.clock) {
            Ok(claims) => {
                let request_id = req
                    .extensions()
                    .get::<RequestId>()
                    .map(|r| r.0.clone())
                    .unwrap_or_default();
                if let Some(slot) = req.extensions().get::<Arc<LogSlot>>() {
                    slot.update(|f| f.principal = Some(claims.sub.id_string()));
                }
                req.extensions_mut().insert(CallerCtx {
                    principal: claims.sub.clone(),
                    claims,
                    request_id,
                });
                Box::pin(inner.oneshot(req))
            }
            Err(e) => {
                tracing::debug!(rpc = rpc_name(req.uri().path()), error = %e, "rejected call");
                Box::pin(std::future::ready(Ok(error_response(&e))))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_auth::Keyring;
    use scion_core::{ManualClock, Principal};
    use std::time::{Duration, SystemTime};

    fn ok_service() -> BoxSvc {
        BoxCloneSyncService::new(tower::service_fn(|req: HttpReq| async move {
            let mut resp = HttpResp::new(tonic::body::Body::empty());
            if let Some(c) = req.extensions().get::<CallerCtx>() {
                resp.headers_mut().insert(
                    "x-caller",
                    http::HeaderValue::from_str(&c.principal.id_string()).unwrap(),
                );
                resp.headers_mut().insert(
                    "x-caller-rid",
                    http::HeaderValue::from_str(&c.request_id).unwrap(),
                );
            }
            Ok::<_, Infallible>(resp)
        }))
    }

    fn req(path: &str, token: Option<&str>) -> HttpReq {
        let mut b = http::Request::builder().uri(format!("http://x{path}"));
        if let Some(t) = token {
            b = b.header("authorization", t);
        }
        b.body(tonic::body::Body::empty()).unwrap()
    }

    #[test]
    fn names_and_tokens() {
        assert_eq!(rpc_name("/scion.v1.AuthService/WhoAmI"), "WhoAmI");
        assert_eq!(rpc_name("weird"), "weird");
        assert!(!requires_token("/scion.v1.AuthService/Login"));
        assert!(!requires_token("/scion.v1.ObjectService/GetServerInfo"));
        assert!(requires_token("/scion.v1.AuthService/WhoAmI"));
        let mut h = http::HeaderMap::new();
        assert_eq!(bearer_token(&h), None);
        h.insert("authorization", "bearer abc".parse().unwrap());
        assert_eq!(bearer_token(&h), Some("abc"));
        h.insert("authorization", "Basic abc".parse().unwrap());
        assert_eq!(bearer_token(&h), None);
        h.insert("authorization", "Bearer ".parse().unwrap());
        assert_eq!(bearer_token(&h), None);
        assert!(acceptable_request_id("abc-1"));
        assert!(!acceptable_request_id("a b"));
        assert!(!acceptable_request_id(&"a".repeat(200)));
    }

    #[tokio::test]
    async fn interceptor_accepts_valid_and_rejects_invalid_tokens() {
        let kr = Keyring::generate_for_tests(1);
        let clock = ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000));
        let svc = RequestIdService::new(BoxCloneSyncService::new(AuthService::new(
            ok_service(),
            kr.verifier(),
            clock.clone(),
        )));
        let alice = Principal::GitHub {
            login: "alice".into(),
            id: Some(7),
        };
        let (tok, _) = kr
            .issue(&alice, Duration::from_secs(60), clock.now())
            .unwrap();

        let r = svc
            .clone()
            .oneshot(req(
                "/scion.v1.AuthService/WhoAmI",
                Some(&format!("Bearer {tok}")),
            ))
            .await
            .unwrap();
        assert_eq!(r.headers()["x-caller"], "github:alice");
        assert_eq!(r.headers()["x-caller-rid"], r.headers()[REQUEST_ID_HEADER]);

        // Exempt paths pass without a token.
        let r = svc
            .clone()
            .oneshot(req("/scion.v1.AuthService/Login", None))
            .await
            .unwrap();
        assert!(r.headers().get("grpc-status").is_none());

        let other = Keyring::generate_for_tests(1);
        let (wrong_key, _) = other
            .issue(&alice, Duration::from_secs(60), clock.now())
            .unwrap();
        let mut tampered = tok.clone().into_bytes();
        let i = tampered.len() / 2;
        tampered[i] = if tampered[i] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();
        for bad in [
            None,
            Some("Bearer not-a-token".to_owned()),
            Some(format!("Bearer {wrong_key}")),
            Some(format!("Bearer {tampered}")),
        ] {
            let r = svc
                .clone()
                .oneshot(req("/scion.v1.AuthService/WhoAmI", bad.as_deref()))
                .await
                .unwrap();
            let s = tonic::Status::from_header_map(r.headers()).unwrap();
            assert_eq!(s.code(), tonic::Code::Unauthenticated, "{bad:?}");
        }
        clock.advance(Duration::from_secs(61));
        let r = svc
            .clone()
            .oneshot(req(
                "/scion.v1.AuthService/WhoAmI",
                Some(&format!("Bearer {tok}")),
            ))
            .await
            .unwrap();
        let s = tonic::Status::from_header_map(r.headers()).unwrap();
        assert_eq!(s.code(), tonic::Code::Unauthenticated);

        // A client request id is kept.
        let mut rq = req("/scion.v1.AuthService/Login", None);
        rq.headers_mut()
            .insert(REQUEST_ID_HEADER, "client-rid-1".parse().unwrap());
        let r = svc.clone().oneshot(rq).await.unwrap();
        assert_eq!(r.headers()[REQUEST_ID_HEADER], "client-rid-1");
    }
}
