//! The authenticated caller of a request (P23).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use scion_auth::Claims;
use scion_core::Principal;

/// Set by the auth interceptor in request extensions.
#[derive(Clone, Debug)]
pub struct CallerCtx {
    /// Authenticated principal.
    pub principal: Principal,
    /// Verified token claims.
    pub claims: Claims,
    /// Request id.
    pub request_id: String,
}

/// The caller of `req` (UNAUTHENTICATED when the interceptor did not set one).
pub fn caller<T>(req: &tonic::Request<T>) -> Result<CallerCtx, tonic::Status> {
    todo!("P23")
}
