//! Daemon startup and handle (SPEC 10; P16).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::upstream::UpstreamConnector;
use scion_cache::DiskCache;
use scion_config::DaemonConfig;
use scion_core::{Clock, Result};
use scion_obs::Metrics;
use scion_proto::Endpoint;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Observable daemon counters and state.
#[derive(Clone, Debug, Default)]
pub struct DaemonStats {
    /// Requests sent upstream.
    pub upstream_requests: u64,
    /// Objects served from cache.
    pub cache_hits: u64,
    /// Objects fetched upstream.
    pub cache_misses: u64,
    /// True while in fail-closed mode (SPEC 13.3).
    pub fail_closed: bool,
    /// Local redaction epoch.
    pub redaction_epoch: u64,
    /// Last observed policy epoch.
    pub policy_epoch: u64,
}

/// Daemon entry point.
pub struct Daemon;

impl Daemon {
    /// Bind the Unix socket (mode 0666) and start serving.
    pub async fn start(
        cfg: DaemonConfig,
        upstream: Arc<dyn UpstreamConnector>,
        clock: Arc<dyn Clock>,
        metrics: Arc<Metrics>,
    ) -> Result<DaemonHandle> {
        todo!("P16")
    }
}

/// A running daemon.
pub struct DaemonHandle {
    socket: PathBuf,
    cache: Arc<DiskCache>,
}

impl DaemonHandle {
    /// The Unix socket path.
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// `unix://` endpoint of the socket.
    pub fn endpoint(&self) -> Endpoint {
        Endpoint::Unix(self.socket.clone())
    }

    /// Current counters and state.
    pub fn stats(&self) -> DaemonStats {
        todo!("P16")
    }

    /// The object cache.
    pub fn cache(&self) -> &DiskCache {
        &self.cache
    }

    /// Test hook: pause redaction polling.
    pub fn block_redaction_polling(&self, on: bool) {
        todo!("P16")
    }

    /// Stop serving and remove the socket.
    pub async fn shutdown(self) {
        todo!("P16")
    }
}
