//! In-process server for tests (feature `testing`; PLAN 3.1, 2.14; P23).
//!
//! [`InProcessServer::start`] runs a real [`ServerApp`] on `127.0.0.1:0` over in-memory storage
//! (or a provided [`Storage`]), with a temporary directory holding `acl.toml`, the Ed25519
//! signing keys as PKCS#8 PEM files and, when `tls` is set, an rcgen CA plus server certificate
//! (amendment O-1). Without TLS the same server is also exposed on a Unix socket in the temp
//! directory, which is what [`InProcessServer::endpoint`] then points at (clients refuse
//! plaintext TCP; SPEC 17).
#![allow(clippy::expect_used, clippy::panic)] // Test harness: failures abort the test.

use crate::app::{AppContext, ServerApp, ServerDeps, ServerHandle};
use scion_auth::{GitHubClient, Keyring};
use scion_config::{ServerConfig, ServicePrincipalConfig};
use scion_core::{Clock, ManualClock, Principal, Result, SystemClock};
use scion_derived::Registry;
use scion_obs::Metrics;
use scion_policy::PolicyHandle;
use scion_proto::{Endpoint, ScionClient, TlsOptions};
use scion_store::{MemHandles, MemOptions, Storage};
use sha2::Digest;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The default test ACL: group `eng` (alice, bob, carol, the `ci`, `agent` and `importer`
/// services and every admin) reads and writes `/` and updates every ref; group `admins`
/// (`github:admin`) may force-update refs and holds every `[admin]` permission.
pub const DEFAULT_ACL: &str = r#"version = 1

[groups]
eng = ["github:alice", "github:bob", "github:carol", "service:ci", "service:agent", "service:importer", "group:admins"]
admins = ["github:admin"]

[[paths]]
prefix = "/"
read = ["group:eng"]
write = ["group:eng"]

[refs."*"]
update = ["group:eng"]
force = ["group:admins"]

[admin]
redact = ["group:admins"]
purge = ["group:admins"]
refs = ["group:admins"]
"#;

/// An unroutable GitHub API used when the test does not supply one (no network in tests).
const NO_GITHUB: &str = "http://127.0.0.1:9";

/// Options of [`InProcessServer::start`] (PLAN 2.14).
#[derive(Clone)]
pub struct ServerTestOpts {
    /// `acl.toml` contents.
    pub acl_toml: String,
    /// Storage to use; `None` = `Storage::in_memory(mem)`.
    pub storage: Option<Storage>,
    /// In-memory options when `storage` is `None`.
    pub mem: MemOptions,
    /// Serve TLS with test certificates.
    pub tls: bool,
    /// Manual clock; `None` = system clock.
    pub clock: Option<Arc<ManualClock>>,
    /// GitHub API URL (e.g. a FakeGitHub).
    pub github_api_url: Option<String>,
    /// Service principals as `(name, plaintext secret)`.
    pub service_principals: Vec<(String, String)>,
    /// Final config tweak.
    pub tweak: Option<Arc<dyn Fn(&mut ServerConfig) + Send + Sync>>,
    /// Registry factory; `None` = `Registry::v1_default()`.
    pub registry: Option<Arc<dyn Fn() -> Registry + Send + Sync>>,
}

impl Default for ServerTestOpts {
    /// ACL: everyone in group `eng` reads/writes `/`; admins = `github:admin`
    /// ([`DEFAULT_ACL`]). TLS on, in-memory storage, system clock, no GitHub.
    fn default() -> Self {
        ServerTestOpts {
            acl_toml: DEFAULT_ACL.to_owned(),
            storage: None,
            mem: MemOptions::default(),
            tls: true,
            clock: None,
            github_api_url: None,
            service_principals: Vec::new(),
            tweak: None,
            registry: None,
        }
    }
}

/// A server on `127.0.0.1:0` over (by default) in-memory storage (PLAN 3.1).
pub struct InProcessServer {
    /// Bound address.
    pub addr: SocketAddr,
    /// Endpoint for clients.
    pub endpoint: Endpoint,
    /// Client TLS options when serving TLS.
    pub tls: Option<TlsOptions>,
    /// Storage (survives kill/restart).
    pub storage: Storage,
    /// In-memory handles when in-memory storage is used.
    pub mem: Option<MemHandles>,
    /// Token signing keys.
    pub keyring: Keyring,
    /// The acl.toml path.
    pub policy_path: PathBuf,
    /// Temp directory holding keys, policy and certificates.
    pub dir: tempfile::TempDir,
    handle: Option<ServerHandle>,
    ctx: Arc<AppContext>,
    opts: ServerTestOpts,
    config: ServerConfig,
    clock: Arc<dyn Clock>,
    tls_pem: Option<(Vec<u8>, Vec<u8>)>,
    unix: Option<PathBuf>,
}

fn sha256_hex(secret: &str) -> String {
    hex::encode(sha2::Sha256::digest(secret.as_bytes()))
}

impl InProcessServer {
    /// Start a server (PLAN 3.1). Panics when the server cannot start.
    pub async fn start(opts: ServerTestOpts) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let (storage, mem) = match &opts.storage {
            Some(s) => (s.clone(), None),
            None => {
                let (s, h) = Storage::in_memory(opts.mem.clone());
                (s, Some(h))
            }
        };
        let keys_dir = dir.path().join("keys");
        std::fs::create_dir_all(&keys_dir).expect("keys dir");
        let generated = Keyring::generate_for_tests(1);
        let key_paths = generated
            .write_pem_files(&keys_dir)
            .expect("write signing keys");
        let policy_path = dir.path().join("acl.toml");
        std::fs::write(&policy_path, &opts.acl_toml).expect("write acl.toml");

        let mut config = ServerConfig::for_tests(dir.path());
        config.auth.signing_keys = key_paths;
        config.policy.path = policy_path.clone();
        let github = opts
            .github_api_url
            .clone()
            .unwrap_or_else(|| NO_GITHUB.to_owned());
        config.auth.github_api_url = github.clone();
        config.auth.github_oauth_base_url = github;
        config.auth.service_principals = opts
            .service_principals
            .iter()
            .map(|(name, secret)| ServicePrincipalConfig {
                name: name.clone(),
                secret_sha256: sha256_hex(secret),
            })
            .collect();
        let tls_pem = if opts.tls {
            let (_ca, cert, key) = crate::tls::generate_test_tls().expect("test TLS material");
            std::fs::write(&config.server.tls_cert, &cert).expect("write tls.crt");
            std::fs::write(&config.server.tls_key, &key).expect("write tls.key");
            Some((_ca, cert, key))
        } else {
            None
        };
        if let Some(t) = &opts.tweak {
            t(&mut config);
        }
        let clock: Arc<dyn Clock> = match &opts.clock {
            Some(c) => c.clone(),
            None => Arc::new(SystemClock),
        };
        let keyring = Keyring::from_files(&config.auth.signing_keys).expect("load signing keys");
        let unix = (!opts.tls).then(|| dir.path().join("grpc.sock"));
        let (ctx, handle) = Self::launch(
            &opts,
            &config,
            &storage,
            &keyring,
            &clock,
            tls_pem.as_ref().map(|(_, c, k)| (c.clone(), k.clone())),
            unix.clone(),
            config.server.listen,
        )
        .await;
        let addr = handle.addr;
        let (endpoint, tls) = match &tls_pem {
            Some((ca, _, _)) => (
                Endpoint::Https(format!("https://127.0.0.1:{}", addr.port())),
                Some(TlsOptions {
                    ca_pem: Some(ca.clone()),
                    domain: None,
                }),
            ),
            None => (
                Endpoint::Unix(unix.clone().expect("unix socket path")),
                None,
            ),
        };
        InProcessServer {
            addr,
            endpoint,
            tls,
            storage,
            mem,
            keyring,
            policy_path,
            dir,
            handle: Some(handle),
            ctx,
            opts,
            config,
            clock,
            tls_pem: tls_pem.map(|(_, c, k)| (c, k)),
            unix,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn launch(
        opts: &ServerTestOpts,
        config: &ServerConfig,
        storage: &Storage,
        keyring: &Keyring,
        clock: &Arc<dyn Clock>,
        tls: Option<(Vec<u8>, Vec<u8>)>,
        unix: Option<PathBuf>,
        listen: SocketAddr,
    ) -> (Arc<AppContext>, ServerHandle) {
        let policy = PolicyHandle::load(&config.policy.path).expect("load acl.toml");
        let deps = ServerDeps {
            storage: storage.clone(),
            config: config.clone(),
            policy,
            keyring: keyring.clone(),
            github: GitHubClient::new(&config.auth.github_api_url),
            clock: clock.clone(),
            metrics: Metrics::new(),
            registry: opts.registry.as_ref().map(|f| f()),
        };
        let app = ServerApp::build(deps).await.expect("build ServerApp");
        let ctx = app.context();
        let listener = bind_retry(listen).await;
        let handle = app.serve_with(listener, tls, unix).await.expect("serve");
        (ctx, handle)
    }

    /// The application context.
    pub fn ctx(&self) -> Arc<AppContext> {
        self.ctx.clone()
    }

    /// A token for `p` issued directly from the keyring at the server clock's now, with the
    /// configured TTL (SPEC 12.1).
    pub fn token_for(&self, p: &Principal) -> String {
        self.keyring
            .issue(p, self.config.auth.token_ttl, self.clock.now())
            .expect("issue token")
            .0
    }

    /// A client, authenticated as `as_` when given.
    pub async fn client(&self, as_: Option<&Principal>) -> ScionClient {
        ScionClient::connect(
            &self.endpoint,
            self.tls.clone(),
            as_.map(|p| self.token_for(p)),
        )
        .await
        .expect("connect to in-process server")
    }

    /// Write the acl file and reload now (SPEC 6.2). An invalid file keeps the last good
    /// policy and is returned as INVALID_ARGUMENT; the server keeps serving.
    pub fn set_policy(&self, toml: &str) -> Result<()> {
        std::fs::write(&self.policy_path, toml).map_err(|e| {
            scion_core::ScionError::unavailable(
                scion_core::subcode::STORE_UNAVAILABLE,
                format!("cannot write acl.toml: {e}"),
            )
        })?;
        crate::reload::reload_now(&self.ctx.policy, &self.ctx.metrics).map(|_| ())
    }

    /// Abort serving tasks and drop the listener; storage survives.
    pub async fn kill(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort_and_wait().await;
        }
    }

    /// Build a new ServerApp over the same storage on the same address.
    pub async fn restart(&mut self) {
        self.kill().await;
        let (ctx, handle) = Self::launch(
            &self.opts,
            &self.config,
            &self.storage,
            &self.keyring,
            &self.clock,
            self.tls_pem.clone(),
            self.unix.clone(),
            self.addr,
        )
        .await;
        assert_eq!(handle.addr, self.addr, "restart must reuse the address");
        self.ctx = ctx;
        self.handle = Some(handle);
    }

    /// The server's admin (metrics/health) address while running.
    pub fn admin_addr(&self) -> Option<SocketAddr> {
        self.handle.as_ref().and_then(|h| h.admin_addr)
    }

    /// The effective server configuration (after `tweak`).
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }
}

impl Drop for InProcessServer {
    fn drop(&mut self) {
        if let Some(h) = &self.handle {
            h.abort();
        }
    }
}

/// Bind `addr`, retrying briefly (a restarted server reuses a port that was just closed).
async fn bind_retry(addr: SocketAddr) -> tokio::net::TcpListener {
    let mut last = None;
    for _ in 0..100 {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => return l,
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    panic!("cannot bind {addr}: {last:?}");
}
