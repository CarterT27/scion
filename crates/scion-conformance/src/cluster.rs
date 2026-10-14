//! The test cluster (P30).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use scion_config::DaemonConfig;
use scion_core::{ObjectId, Principal, Result};
use scion_daemon::testing::InProcessDaemon;
use scion_obs::capture::LogCapture;
use scion_proto::ScionClient;
use scion_server::testing::{InProcessServer, ServerTestOpts};
use scion_store::{CounterSnapshot, Storage};
use scion_testkit::{FakeGitHub, GenRepo};
use std::sync::Arc;

/// How a client reaches the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// Directly over TLS.
    Server,
    /// Through the daemon's Unix socket.
    Daemon,
}

/// Storage backend of a cluster.
#[derive(Clone, Debug)]
pub enum Backend {
    /// In-memory (always available).
    Memory,
    /// CockroachDB + S3, only when `SCION_TEST_CRDB_URL` and `SCION_TEST_S3_ENDPOINT` are set.
    External,
}

impl Backend {
    /// Always `[Memory]`, plus `External` when available.
    pub fn from_env() -> Vec<Backend> {
        todo!("P30")
    }
}

/// Cluster options.
pub struct ClusterOpts {
    /// Backend.
    pub backend: Backend,
    /// Number of servers sharing the storage.
    pub servers: usize,
    /// Start a daemon in front of `servers[0]`.
    pub daemon: bool,
    /// `acl.toml` contents.
    pub acl_toml: String,
    /// Base server options.
    pub server: ServerTestOpts,
    /// Daemon config tweak.
    pub daemon_tweak: Option<Arc<dyn Fn(&mut DaemonConfig) + Send + Sync>>,
}

/// Servers, an optional daemon, a fake GitHub and a log capture over shared storage.
pub struct TestCluster {
    /// Servers.
    pub servers: Vec<InProcessServer>,
    /// Daemon.
    pub daemon: Option<InProcessDaemon>,
    /// Fake GitHub.
    pub github: FakeGitHub,
    /// Shared storage.
    pub storage: Storage,
    /// Captured logs.
    pub logs: LogCapture,
}

impl TestCluster {
    /// Start a cluster.
    pub async fn start(o: ClusterOpts) -> Self {
        todo!("P30")
    }

    /// A client authenticated as `who`.
    pub async fn client(&self, via: Via, who: &Principal) -> ScionClient {
        todo!("P30")
    }

    /// A token for `who`.
    pub fn token_for(&self, who: &Principal) -> String {
        todo!("P30")
    }

    /// Replace the policy on every server.
    pub fn set_policy(&self, toml: &str) -> Result<()> {
        todo!("P30")
    }

    /// Upload and create a generated history through RPCs; returns changeset ids by index.
    pub async fn seed(&self, repo: &GenRepo) -> Vec<ObjectId> {
        todo!("P30")
    }

    /// Storage counters snapshot.
    pub fn counters(&self) -> CounterSnapshot {
        todo!("P30")
    }

    /// Kill server `i` (storage survives).
    pub async fn kill_server(&mut self, i: usize) {
        todo!("P30")
    }

    /// Restart server `i`.
    pub async fn restart_server(&mut self, i: usize) {
        todo!("P30")
    }
}
