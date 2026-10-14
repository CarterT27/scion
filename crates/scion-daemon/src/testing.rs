//! In-process daemon for tests (feature `testing`; P16).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::daemon::DaemonHandle;
use scion_config::DaemonConfig;
use scion_proto::{Endpoint, ScionClient, TlsOptions};

/// A daemon on a temp Unix socket in front of `upstream`.
pub struct InProcessDaemon {
    /// The running daemon.
    pub handle: DaemonHandle,
    /// Temp directory holding the socket and cache.
    pub dir: tempfile::TempDir,
    /// The configuration used.
    pub cfg: DaemonConfig,
}

impl InProcessDaemon {
    /// Start with `DaemonConfig::for_tests` adjusted by `tweak`.
    pub async fn start(
        upstream: Endpoint,
        tls: Option<TlsOptions>,
        tweak: impl FnOnce(&mut DaemonConfig),
    ) -> Self {
        todo!("P16")
    }

    /// A client over the socket.
    pub async fn client(&self, token: Option<String>) -> ScionClient {
        todo!("P16")
    }

    /// Stop the daemon; the cache directory survives.
    pub async fn kill(&mut self) {
        todo!("P16")
    }

    /// Start again with the same configuration and cache.
    pub async fn restart(&mut self) {
        todo!("P16")
    }
}
