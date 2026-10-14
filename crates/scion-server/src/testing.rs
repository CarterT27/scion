//! In-process server for tests (feature `testing`; PLAN 3.1; P23).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::app::{AppContext, ServerHandle};
use scion_auth::Keyring;
use scion_config::ServerConfig;
use scion_core::{ManualClock, Principal, Result};
use scion_derived::Registry;
use scion_proto::{Endpoint, ScionClient, TlsOptions};
use scion_store::{MemHandles, MemOptions, Storage};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

/// Options of [`InProcessServer::start`].
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
    /// ACL: everyone in group `eng` reads/writes `/`; admins = `github:admin`.
    fn default() -> Self {
        todo!("P23")
    }
}

/// A server on `127.0.0.1:0` over (by default) in-memory storage.
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
}

impl InProcessServer {
    /// Start a server.
    pub async fn start(opts: ServerTestOpts) -> Self {
        todo!("P23")
    }

    /// The application context.
    pub fn ctx(&self) -> Arc<AppContext> {
        todo!("P23")
    }

    /// A token for `p` issued directly from the keyring.
    pub fn token_for(&self, p: &Principal) -> String {
        todo!("P23")
    }

    /// A client, authenticated as `as_` when given.
    pub async fn client(&self, as_: Option<&Principal>) -> ScionClient {
        todo!("P23")
    }

    /// Write the acl file and reload now.
    pub fn set_policy(&self, toml: &str) -> Result<()> {
        todo!("P23")
    }

    /// Abort serving tasks and drop the listener; storage survives.
    pub async fn kill(&mut self) {
        todo!("P23")
    }

    /// Build a new ServerApp over the same storage on the same address.
    pub async fn restart(&mut self) {
        todo!("P23")
    }
}
