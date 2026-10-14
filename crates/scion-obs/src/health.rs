//! Health and admin endpoints (SPEC 15.3; P04).

use crate::metrics::Metrics;
use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use std::net::SocketAddr;
use std::sync::Arc;

/// Readiness probe of the process's dependencies (SPEC 15.3: dependencies reachable, policy
/// loaded).
pub trait Readiness: Send + Sync {
    /// `Ok` when ready; `Err(reason)` otherwise.
    fn check(&self) -> Result<(), String>;
}

/// The Prometheus text exposition content type.
const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// `/metrics` (text exposition), `/healthz` (200 while alive), `/readyz` (200 or 503)
/// (SPEC 15.2, 15.3).
pub fn health_router(metrics: Arc<Metrics>, ready: Arc<dyn Readiness>) -> axum::Router {
    Router::new()
        .route(
            "/metrics",
            get(move || {
                let metrics = metrics.clone();
                async move {
                    (
                        [(header::CONTENT_TYPE, METRICS_CONTENT_TYPE)],
                        metrics.render(),
                    )
                }
            }),
        )
        .route("/healthz", get(|| async { (StatusCode::OK, "ok\n") }))
        .route(
            "/readyz",
            get(move || {
                let ready = ready.clone();
                async move {
                    match ready.check() {
                        Ok(()) => (StatusCode::OK, "ready\n".to_string()).into_response(),
                        Err(reason) => (
                            StatusCode::SERVICE_UNAVAILABLE,
                            format!("not ready: {reason}\n"),
                        )
                            .into_response(),
                    }
                }
            }),
        )
}

/// Bind `addr` and serve `router`; returns the bound address and the server task
/// (SPEC 15.3: health endpoints on the metrics listener).
pub async fn serve_admin(
    addr: SocketAddr,
    router: axum::Router,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!(target: "scion::obs", error = %e, "admin listener failed");
        }
    });
    Ok((local, handle))
}
