//! The authoritative Scion server library (SPEC 3.1, 7; PLAN 1.4, 2.14).
//!
//! [`ServerApp::build`] wires storage, the object facade, history, derived data, redaction,
//! policy, auth and observability; [`grpc`] implements the six `scion.v1` services by
//! delegating to [`handlers`]. The `testing` feature adds [`testing::InProcessServer`].
#![warn(missing_docs)]

pub mod authz;
pub mod context;
pub mod grpc;
pub mod handlers;

mod app;
mod errors;
mod interceptor;
mod jobs;
mod logging;
mod ratelimit;
mod redaction_set;
mod reload;
mod revision;
mod tls;

#[cfg(feature = "testing")]
pub mod testing;

pub use app::{AppContext, ServerApp, ServerDeps, ServerHandle};
pub use context::CallerCtx;
pub use redaction_set::RedactionSet;
