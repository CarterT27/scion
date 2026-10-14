//! The test cluster (PLAN 2.22, 3.1, 3.3; SPEC 19.0; P30).
//!
//! A [`TestCluster`] is the hermetic stack every cross-cutting SPEC 19 suite runs against:
//! `N` in-process servers (TLS on loopback) sharing one [`Storage`], an optional in-process
//! daemon in front of `servers[0]`, a [`FakeGitHub`], and the process-global [`LogCapture`].
//! All servers verify the same signing keys, so a token issued for one is accepted by all of
//! them and by the daemon.
// SAFETY-ish: this is a test harness with frozen infallible signatures (PLAN 2.22); a cluster
// that cannot start, connect or seed cannot run any suite, so failing loudly is its report.
#![allow(clippy::expect_used, clippy::panic)]

use scion_config::DaemonConfig;
use scion_core::{FaultRegistry, ObjectId, Principal, Result};
use scion_daemon::testing::InProcessDaemon;
use scion_obs::capture::LogCapture;
use scion_proto::ScionClient;
use scion_server::testing::{InProcessServer, ServerTestOpts};
use scion_store::{CounterSnapshot, Storage};
use scion_testkit::{FakeGitHub, GenRepo};
use std::sync::Arc;

/// How a client reaches the server (SPEC 3.3, 19.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// Directly over TLS.
    Server,
    /// Through the daemon's Unix socket.
    Daemon,
}

impl Via {
    /// Both tiers, server first (SPEC 19.8: one suite runs unchanged against both).
    pub const ALL: [Via; 2] = [Via::Server, Via::Daemon];

    /// `"server"` or `"daemon"` (for messages and test names).
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Server => "server",
            Via::Daemon => "daemon",
        }
    }
}

/// Storage backend of a cluster (PLAN 3.3).
#[derive(Clone, Debug)]
pub enum Backend {
    /// In-memory (always available).
    Memory,
    /// CockroachDB + S3, only when `SCION_TEST_CRDB_URL` and `SCION_TEST_S3_ENDPOINT` are set.
    External,
}

impl Backend {
    /// Always `[Memory]`, plus `External` when both `SCION_TEST_CRDB_URL` and
    /// `SCION_TEST_S3_ENDPOINT` are set (PLAN 3.3).
    pub fn from_env() -> Vec<Backend> {
        let mut out = vec![Backend::Memory];
        if Self::external_available() {
            out.push(Backend::External);
        }
        out
    }

    /// Whether the External backend's environment gates are set (PLAN 3.3).
    pub fn external_available() -> bool {
        scion_store_crdb::test_url().is_some() && scion_store_s3::test_config().is_some()
    }

    /// `"memory"` or `"external"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Memory => "memory",
            Backend::External => "external",
        }
    }
}

/// Cluster options (PLAN 2.22).
pub struct ClusterOpts {
    /// Backend.
    pub backend: Backend,
    /// Number of servers sharing the storage.
    pub servers: usize,
    /// Start a daemon in front of `servers[0]`.
    pub daemon: bool,
    /// `acl.toml` contents (overrides `server.acl_toml`).
    pub acl_toml: String,
    /// Base server options (`storage` and `github_api_url` are set by the cluster).
    pub server: ServerTestOpts,
    /// Daemon config tweak.
    pub daemon_tweak: Option<Arc<dyn Fn(&mut DaemonConfig) + Send + Sync>>,
}

impl ClusterOpts {
    /// One server, a daemon, [`TestCluster::DEFAULT_ACL`], the default service principals
    /// ([`TestCluster::SERVICES`]) and rate limits raised far above any suite's load.
    pub fn new(backend: Backend) -> Self {
        ClusterOpts {
            backend,
            servers: 1,
            daemon: true,
            acl_toml: TestCluster::DEFAULT_ACL.to_owned(),
            server: ServerTestOpts {
                service_principals: TestCluster::SERVICES
                    .iter()
                    .map(|(n, s)| ((*n).to_owned(), (*s).to_owned()))
                    .collect(),
                tweak: Some(Arc::new(|c| {
                    c.rate_limit.per_principal_rps = 1_000_000;
                    c.rate_limit.burst = 1_000_000;
                    c.rate_limit.max_inflight = 1_000_000;
                })),
                ..ServerTestOpts::default()
            },
            daemon_tweak: None,
        }
    }

    /// Replace the ACL.
    pub fn with_acl(mut self, acl: &str) -> Self {
        self.acl_toml = acl.to_owned();
        self
    }

    /// Set the number of servers.
    pub fn with_servers(mut self, n: usize) -> Self {
        self.servers = n;
        self
    }

    /// Start (or not) the daemon.
    pub fn with_daemon(mut self, on: bool) -> Self {
        self.daemon = on;
        self
    }
}

impl Default for ClusterOpts {
    /// [`ClusterOpts::new`] on the in-memory backend.
    fn default() -> Self {
        ClusterOpts::new(Backend::Memory)
    }
}

/// Servers, an optional daemon, a fake GitHub and a log capture over shared storage
/// (PLAN 2.22, SPEC 19.0).
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

/// Build the shared storage of `backend` (PLAN 3.3: a fresh CockroachDB database and a unique
/// S3 key prefix per cluster).
/// `None` for the in-memory backend: `servers[0]` then builds it, which exposes its
/// [`scion_store::MemHandles`] as `servers[0].mem` for fault tests.
async fn external_storage(backend: &Backend) -> Option<Storage> {
    match backend {
        Backend::Memory => None,
        Backend::External => {
            let url = scion_store_crdb::test_url().expect("SCION_TEST_CRDB_URL is set");
            let meta = scion_store_crdb::CrdbMetaStore::connect_fresh_for_tests(&url)
                .await
                .expect("connect to the test CockroachDB");
            let cfg = scion_store_s3::test_config().expect("SCION_TEST_S3_ENDPOINT is set");
            let blobs = scion_store_s3::S3BlobStore::new(cfg).expect("S3 test configuration");
            Some(Storage::new(
                Arc::new(meta),
                Arc::new(blobs),
                FaultRegistry::new(),
            ))
        }
    }
}

impl TestCluster {
    /// Start a cluster (PLAN 2.22): the shared storage of `o.backend`, `o.servers` servers
    /// over it (all verifying `servers[0]`'s signing keys), a daemon in front of `servers[0]`
    /// when `o.daemon`, a [`FakeGitHub`] wired into every server, and the log capture.
    pub async fn start(o: ClusterOpts) -> Self {
        let logs = LogCapture::install();
        let github = FakeGitHub::start().await;
        let mut storage = external_storage(&o.backend).await;
        let n = o.servers.max(1);
        let mut servers: Vec<InProcessServer> = Vec::with_capacity(n);
        for i in 0..n {
            let mut opts = o.server.clone();
            opts.acl_toml = o.acl_toml.clone();
            opts.storage = storage.clone();
            opts.github_api_url = Some(github.api_url());
            if i > 0 {
                // Every server verifies (and signs with) the first server's keys.
                let keys = servers[0].config().auth.signing_keys.clone();
                let user = opts.tweak.clone();
                opts.tweak = Some(Arc::new(move |c| {
                    if let Some(t) = &user {
                        t(c);
                    }
                    c.auth.signing_keys = keys.clone();
                }));
            }
            let srv = InProcessServer::start(opts).await;
            storage.get_or_insert_with(|| srv.storage.clone());
            servers.push(srv);
        }
        let storage = storage.expect("the first server created the storage");
        let daemon = if o.daemon {
            let tweak = o.daemon_tweak.clone();
            Some(
                InProcessDaemon::start(
                    servers[0].endpoint.clone(),
                    servers[0].tls.clone(),
                    move |c| {
                        if let Some(t) = &tweak {
                            t(c);
                        }
                    },
                )
                .await,
            )
        } else {
            None
        };
        TestCluster {
            servers,
            daemon,
            github,
            storage,
            logs,
        }
    }

    /// A client authenticated as `who`, connected directly to `servers[0]` or through the
    /// daemon (SPEC 7.1). Panics for `Via::Daemon` on a cluster without a daemon.
    pub async fn client(&self, via: Via, who: &Principal) -> ScionClient {
        self.client_with_token(via, Some(self.token_for(who))).await
    }

    /// A token for `who`, issued with the cluster's shared signing key (SPEC 12.1).
    pub fn token_for(&self, who: &Principal) -> String {
        self.servers[0].token_for(who)
    }

    /// Replace the policy on every server (SPEC 6.2 reload). The first error is returned;
    /// an invalid policy leaves every server on its last good policy.
    pub fn set_policy(&self, toml: &str) -> Result<()> {
        let mut first = None;
        for s in &self.servers {
            if let Err(e) = s.set_policy(toml) {
                first.get_or_insert(e);
            }
        }
        match first {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Upload and create a generated history through RPCs as [`TestCluster::seeder`];
    /// returns changeset ids by commit index (SPEC 19.0 corpus).
    pub async fn seed(&self, repo: &GenRepo) -> Vec<ObjectId> {
        let client = self.client(Via::Server, &Self::seeder()).await;
        crate::seed::seed_repo(&client, repo)
            .await
            .unwrap_or_else(|e| panic!("seeding a generated repository failed: {e}"))
    }

    /// Storage counters snapshot (SPEC 19.0 storage access counters).
    pub fn counters(&self) -> CounterSnapshot {
        self.storage.counters.snapshot()
    }

    /// Kill server `i` (storage survives; SPEC 16.1).
    pub async fn kill_server(&mut self, i: usize) {
        self.servers[i].kill().await;
    }

    /// Restart server `i` over the same storage and address.
    pub async fn restart_server(&mut self, i: usize) {
        self.servers[i].restart().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_always_has_memory_first() {
        let b = Backend::from_env();
        assert!(matches!(b[0], Backend::Memory));
        assert_eq!(b.len(), 1 + usize::from(Backend::external_available()));
    }

    #[test]
    fn default_options() {
        let o = ClusterOpts::default();
        assert!(matches!(o.backend, Backend::Memory));
        assert_eq!((o.servers, o.daemon), (1, true));
        assert_eq!(o.acl_toml, TestCluster::DEFAULT_ACL);
        assert_eq!(
            o.server.service_principals.len(),
            TestCluster::SERVICES.len()
        );
        let o = o.with_servers(3).with_daemon(false).with_acl("x");
        assert_eq!((o.servers, o.daemon, o.acl_toml.as_str()), (3, false, "x"));
        assert_eq!(Via::ALL.map(Via::as_str), ["server", "daemon"]);
    }
}
