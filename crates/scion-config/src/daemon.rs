//! `scion-daemon.toml` (SPEC 6.3 plus PLAN 6.11 keys; P04).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::env::Env;
use crate::error::ConfigError;
use std::path::{Path, PathBuf};

/// The daemon configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct DaemonConfig {
    /// Unix socket path.
    pub socket: PathBuf,
    /// Upstream server URL.
    pub upstream: String,
    /// Cache directory.
    pub cache_dir: PathBuf,
    /// Default 50 GiB.
    pub max_cache_bytes: u64,
    /// Default 0.8.
    pub low_watermark_ratio: f64,
    /// Default true.
    pub verify_on_read: bool,
    /// Default 1000.
    pub ref_cache_ttl_ms: u64,
    /// Default 60000.
    pub access_cache_ttl_ms: u64,
    /// Default 10000.
    pub redaction_poll_interval_ms: u64,
    /// Default 600000.
    pub redaction_max_staleness_ms: u64,
    /// Default 256.
    pub upstream_max_concurrency: usize,
    /// (+) Default 4096.
    pub upstream_queue: usize,
    /// (+) Optional CA path for the upstream TLS connection.
    pub upstream_ca: Option<PathBuf>,
}

/// Parse daemon TOML text.
pub fn parse_daemon(text: &str, env: &dyn Env) -> Result<DaemonConfig, ConfigError> {
    todo!("P04")
}

/// Read and parse a daemon config file.
pub fn load_daemon(p: &Path, env: &dyn Env) -> Result<DaemonConfig, ConfigError> {
    todo!("P04")
}

impl DaemonConfig {
    /// A complete config for in-process tests with paths under `dir`.
    pub fn for_tests(dir: &Path, upstream: &str) -> Self {
        todo!("P04")
    }
}
