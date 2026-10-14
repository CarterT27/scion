//! `scion-server.toml` (SPEC 6.1 plus the PLAN 6.11 keys marked (+); P04).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::env::Env;
use crate::error::ConfigError;
use scion_core::Limits;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The server configuration: sections exactly as SPEC 6.1 plus `[rate_limit]` (+).
#[derive(Clone, Debug, PartialEq)]
pub struct ServerConfig {
    /// `[server]`.
    pub server: ServerSection,
    /// `[metadata]`.
    pub metadata: MetadataSection,
    /// `[objects]`.
    pub objects: ObjectsSection,
    /// `[auth]`.
    pub auth: AuthSection,
    /// `[policy]`.
    pub policy: PolicySection,
    /// `[limits]`.
    pub limits: Limits,
    /// `[land]`.
    pub land: LandSection,
    /// `[derive]`.
    pub derive: DeriveSection,
    /// `[redaction]`.
    pub redaction: RedactionSection,
    /// `[observability]`.
    pub observability: ObservabilitySection,
    /// `[rate_limit]` (+).
    pub rate_limit: RateLimitSection,
}

/// `[server]`.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerSection {
    /// Listen address.
    pub listen: SocketAddr,
    /// TLS certificate PEM path.
    pub tls_cert: PathBuf,
    /// TLS key PEM path.
    pub tls_key: PathBuf,
    /// Repository name.
    pub repo: String,
}

/// `[metadata]`.
#[derive(Clone, Debug, PartialEq)]
pub struct MetadataSection {
    /// CockroachDB URL.
    pub url: String,
    /// Pool size (default 64).
    pub max_connections: usize,
}

/// `[objects]`.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectsSection {
    /// S3 endpoint.
    pub endpoint: String,
    /// Bucket.
    pub bucket: String,
    /// Key prefix.
    pub prefix: String,
    /// Inline threshold (default 65536).
    pub inline_threshold_bytes: usize,
    /// zstd level (default 3).
    pub zstd_level: i32,
    /// (+) Region (default `us-east-1`).
    pub region: String,
    /// (+) Path-style addressing (default true).
    pub path_style: bool,
}

/// `[auth]`.
#[derive(Clone, Debug, PartialEq)]
pub struct AuthSection {
    /// GitHub OAuth App client id.
    pub github_client_id: String,
    /// GitHub REST API base URL.
    pub github_api_url: String,
    /// (+) GitHub OAuth base URL (default `https://github.com`).
    pub github_oauth_base_url: String,
    /// Token TTL (default 12h).
    pub token_ttl: Duration,
    /// Signing key PEM paths; the first signs, all verify.
    pub signing_keys: Vec<PathBuf>,
    /// `[[auth.service_principals]]`.
    pub service_principals: Vec<ServicePrincipalConfig>,
}

/// `[[auth.service_principals]]`.
#[derive(Clone, Debug, PartialEq)]
pub struct ServicePrincipalConfig {
    /// Service name.
    pub name: String,
    /// SHA-256 of the secret, lowercase hex.
    pub secret_sha256: String,
}

/// `[policy]`.
#[derive(Clone, Debug, PartialEq)]
pub struct PolicySection {
    /// `acl.toml` path.
    pub path: PathBuf,
    /// Watch and hot-reload.
    pub reload: bool,
}

/// `[land]`.
#[derive(Clone, Debug, PartialEq)]
pub struct LandSection {
    /// Default 32.
    pub max_attempts: u32,
    /// Default 5.
    pub backoff_base_ms: u64,
    /// Default 500.
    pub backoff_max_ms: u64,
}

/// `[derive]`.
#[derive(Clone, Debug, PartialEq)]
pub struct DeriveSection {
    /// Default 30000.
    pub lease_ttl_ms: u64,
    /// Default 16.
    pub lazy_workers: usize,
}

/// `[redaction]`.
#[derive(Clone, Debug, PartialEq)]
pub struct RedactionSection {
    /// Default 1000.
    pub propagation_bound_ms: u64,
    /// Default 7d.
    pub default_quarantine: Duration,
}

/// `[observability]`.
#[derive(Clone, Debug, PartialEq)]
pub struct ObservabilitySection {
    /// `json` or `text`.
    pub log_format: String,
    /// Admin listener (metrics/health).
    pub metrics_listen: SocketAddr,
}

/// `[rate_limit]` (+).
#[derive(Clone, Debug, PartialEq)]
pub struct RateLimitSection {
    /// Default 500.
    pub per_principal_rps: u32,
    /// Default 1000.
    pub burst: u32,
    /// Default 4096.
    pub max_inflight: usize,
}

/// Parse server TOML text.
pub fn parse_server(text: &str, env: &dyn Env) -> Result<ServerConfig, ConfigError> {
    todo!("P04")
}

/// Read and parse a server config file.
pub fn load_server(p: &Path, env: &dyn Env) -> Result<ServerConfig, ConfigError> {
    todo!("P04")
}

impl ServerConfig {
    /// A complete config for in-process tests with paths under `dir`.
    pub fn for_tests(dir: &Path) -> Self {
        todo!("P04")
    }
}
