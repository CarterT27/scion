//! In-process daemon for tests (feature `testing`; SPEC 19.0; P16).

use crate::daemon::{Daemon, DaemonHandle};
use crate::upstream::DirectUpstream;
use scion_config::DaemonConfig;
use scion_core::SystemClock;
use scion_obs::Metrics;
use scion_proto::{Endpoint, ScionClient, TlsOptions};
use std::sync::Arc;

/// A daemon on a temp Unix socket in front of `upstream` (SPEC 19.0).
pub struct InProcessDaemon {
    /// The running daemon.
    pub handle: DaemonHandle,
    /// Temp directory holding the socket and cache.
    pub dir: tempfile::TempDir,
    /// The configuration used.
    pub cfg: DaemonConfig,
}

impl InProcessDaemon {
    /// Start with `DaemonConfig::for_tests` adjusted by `tweak`, a [`DirectUpstream`] to
    /// `upstream`, the system clock and a fresh metrics registry (SPEC 19.0).
    // SAFETY-ish: a test fixture that cannot create its temp directory or bind its socket
    // cannot proceed; panicking is the fixture's failure report.
    #[allow(clippy::expect_used)]
    pub async fn start(
        upstream: Endpoint,
        tls: Option<TlsOptions>,
        tweak: impl FnOnce(&mut DaemonConfig),
    ) -> Self {
        let dir = tempfile::tempdir().expect("create daemon temp dir");
        let mut cfg = DaemonConfig::for_tests(dir.path(), &upstream.to_string());
        tweak(&mut cfg);
        let conn = Arc::new(DirectUpstream::new(upstream, tls));
        let handle = Daemon::start(cfg.clone(), conn, Arc::new(SystemClock), Metrics::new())
            .await
            .expect("start in-process daemon");
        InProcessDaemon { handle, dir, cfg }
    }

    /// A client over the socket, sending `token` (SPEC 7.1).
    // SAFETY-ish: see `start`; a test client that cannot connect cannot proceed.
    #[allow(clippy::expect_used)]
    pub async fn client(&self, token: Option<String>) -> ScionClient {
        ScionClient::connect(&self.handle.endpoint(), None, token)
            .await
            .expect("connect to in-process daemon")
    }

    /// Stop the daemon; the cache directory survives (SPEC 16.1 class 6).
    pub async fn kill(&mut self) {
        self.handle.stop().await;
    }

    /// Start again with the same configuration, cache, upstream connector, clock and metrics
    /// (SPEC 16.1 class 6: temp files are deleted at startup, entries stay valid).
    // SAFETY-ish: see `start`.
    #[allow(clippy::expect_used)]
    pub async fn restart(&mut self) {
        self.handle.stop().await;
        let handle = Daemon::start(
            self.cfg.clone(),
            self.handle.upstream(),
            self.handle.clock(),
            self.handle.metrics(),
        )
        .await
        .expect("restart in-process daemon");
        self.handle = handle;
    }
}
