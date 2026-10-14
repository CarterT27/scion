//! The per-node caching daemon (SPEC 3.1, 10; PLAN 2.16, 6.13): a Unix-socket ObjectService
//! served from a verified cache, verbatim forwarding of every other RPC, redaction polling and
//! fail-closed mode. The `testing` feature adds [`testing::InProcessDaemon`].
#![warn(missing_docs)]

mod daemon;
mod upstream;

#[cfg(feature = "testing")]
pub mod testing;

pub use daemon::{Daemon, DaemonHandle, DaemonStats};
pub use upstream::{DirectUpstream, UpstreamConnector};
