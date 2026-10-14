//! Health and admin endpoints (SPEC 15.3; P04).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::metrics::Metrics;
use std::net::SocketAddr;
use std::sync::Arc;

/// Readiness probe of the process's dependencies.
pub trait Readiness: Send + Sync {
    /// `Ok` when ready; `Err(reason)` otherwise.
    fn check(&self) -> Result<(), String>;
}

/// `/metrics` (text exposition), `/healthz` (200 while alive), `/readyz` (200 or 503).
pub fn health_router(metrics: Arc<Metrics>, ready: Arc<dyn Readiness>) -> axum::Router {
    todo!("P04")
}

/// Bind `addr` and serve `router`; returns the bound address and the server task.
pub async fn serve_admin(
    addr: SocketAddr,
    router: axum::Router,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    todo!("P04")
}
