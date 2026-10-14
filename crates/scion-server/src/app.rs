//! Application wiring (PLAN 1.4; P23).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::authz::ObjectAuthorizer;
use crate::redaction_set::RedactionSet;
use scion_auth::{GitHubClient, Keyring, ServicePrincipals, Verifier};
use scion_config::ServerConfig;
use scion_core::{Clock, Result, Tier};
use scion_derived::{DerivedEngine, Registry};
use scion_history::History;
use scion_obs::Metrics;
use scion_policy::PolicyHandle;
use scion_proto::{EpochSource, Epochs};
use scion_redaction::Redactor;
use scion_store::{ObjectRepo, Storage};
use std::net::SocketAddr;
use std::sync::Arc;

/// Inputs of [`ServerApp::build`].
pub struct ServerDeps {
    /// Storage bundle (CRDB+S3 in production, in-memory in tests).
    pub storage: Storage,
    /// Parsed server configuration.
    pub config: ServerConfig,
    /// Loaded policy.
    pub policy: Arc<PolicyHandle>,
    /// Token signing keys.
    pub keyring: Keyring,
    /// GitHub API client.
    pub github: GitHubClient,
    /// Time source.
    pub clock: Arc<dyn Clock>,
    /// Metrics registry.
    pub metrics: Arc<Metrics>,
    /// Derived type registry; `None` = `Registry::v1_default()`.
    pub registry: Option<Registry>,
}

/// Everything a handler needs.
pub struct AppContext {
    /// Storage bundle.
    pub storage: Storage,
    /// Object facade.
    pub objects: ObjectRepo,
    /// History engine.
    pub history: Arc<History>,
    /// Derived data engine.
    pub derived: Arc<DerivedEngine>,
    /// Redaction lifecycle.
    pub redactor: Arc<Redactor>,
    /// In-memory redaction set (SPEC 13.3).
    pub redactions: Arc<RedactionSet>,
    /// Policy.
    pub policy: Arc<PolicyHandle>,
    /// Token keys.
    pub keyring: Keyring,
    /// Token verifier.
    pub verifier: Verifier,
    /// Service principals.
    pub services: ServicePrincipals,
    /// GitHub API client.
    pub github: GitHubClient,
    /// Time source.
    pub clock: Arc<dyn Clock>,
    /// Metrics.
    pub metrics: Arc<Metrics>,
    /// Configuration.
    pub config: ServerConfig,
    /// Object read authorization (SPEC 18.6).
    pub authz: ObjectAuthorizer,
}

impl EpochSource for AppContext {
    fn epochs(&self) -> Epochs {
        Epochs {
            policy: self.policy.epoch(),
            redaction: self.redactions.epoch(),
            tier: Tier::Server,
        }
    }
}

/// A built server.
pub struct ServerApp {
    ctx: Arc<AppContext>,
}

impl ServerApp {
    /// Bootstrap the repository and wire every component, background task and the policy
    /// watcher.
    pub async fn build(deps: ServerDeps) -> Result<Self> {
        todo!("P23")
    }

    /// The application context.
    pub fn context(&self) -> Arc<AppContext> {
        self.ctx.clone()
    }

    /// The gRPC router: the six services plus every layer (PLAN 1.4).
    pub fn router(&self) -> tonic::transport::server::Router {
        todo!("P23")
    }

    /// Serve on `listener`, with TLS when `tls = Some((cert_pem, key_pem))`, plus the admin
    /// listener.
    pub async fn serve(
        self,
        listener: tokio::net::TcpListener,
        tls: Option<(Vec<u8>, Vec<u8>)>,
    ) -> Result<ServerHandle> {
        todo!("P23")
    }
}

/// A running server.
pub struct ServerHandle {
    /// gRPC address.
    pub addr: SocketAddr,
    /// Admin (metrics/health) address.
    pub admin_addr: Option<SocketAddr>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ServerHandle {
    /// Graceful shutdown.
    pub async fn shutdown(self) {
        todo!("P23")
    }

    /// Abort every task immediately.
    pub fn abort(&self) {
        todo!("P23")
    }
}
