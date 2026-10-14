//! The authenticated caller of a request (SPEC 7.1, 12.1; P23).

use scion_auth::Claims;
use scion_core::{Principal, ScionError};

/// Set by the auth interceptor in request extensions (SPEC 7.1): the verified principal, its
/// token claims and the request id.
#[derive(Clone, Debug)]
pub struct CallerCtx {
    /// Authenticated principal.
    pub principal: Principal,
    /// Verified token claims.
    pub claims: Claims,
    /// Request id.
    pub request_id: String,
}

/// The caller of `req` (SPEC 7.1, 12.1): UNAUTHENTICATED when the interceptor did not set one,
/// which happens only for the unauthenticated RPCs `GetServerInfo` and `Login`.
pub fn caller<T>(req: &tonic::Request<T>) -> Result<CallerCtx, tonic::Status> {
    req.extensions().get::<CallerCtx>().cloned().ok_or_else(|| {
        crate::errors::to_status(&ScionError::unauthenticated(
            "missing or invalid bearer token",
        ))
    })
}
