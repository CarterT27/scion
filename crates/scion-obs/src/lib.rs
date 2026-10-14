//! Observability (SPEC 15; PLAN 2.12, 6.9, 6.10): structured logging, RPC log events, path
//! hashing, Prometheus metrics, health endpoints and an in-memory log capture for tests.

pub mod capture;

mod health;
mod logging;
mod metrics;
mod rpc_log;

pub use health::{Readiness, health_router, serve_admin};
pub use logging::{LogFormat, LogGuard, init_logging};
pub use metrics::Metrics;
pub use rpc_log::{RpcLogEvent, path_hash};
