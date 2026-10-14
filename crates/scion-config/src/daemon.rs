//! `scion-daemon.toml` (SPEC 6.3 plus PLAN 6.11 keys; P04).
//!
//! `socket`, `upstream` and `cache_dir` are required; every other key takes its SPEC 6.5 /
//! PLAN 2.11 / PLAN 6.11 default when omitted. Unknown keys are rejected (SPEC 6).

use crate::env::{Env, decode, read_file};
use crate::error::ConfigError;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The daemon configuration (SPEC 6.3).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// Unix socket path.
    pub socket: PathBuf,
    /// Upstream server URL.
    pub upstream: String,
    /// Cache directory.
    pub cache_dir: PathBuf,
    /// Default 50 GiB.
    #[serde(default = "default_max_cache_bytes")]
    pub max_cache_bytes: u64,
    /// Default 0.8.
    #[serde(default = "default_low_watermark_ratio")]
    pub low_watermark_ratio: f64,
    /// Default true.
    #[serde(default = "default_true")]
    pub verify_on_read: bool,
    /// Default 1000.
    #[serde(default = "default_ref_cache_ttl_ms")]
    pub ref_cache_ttl_ms: u64,
    /// Default 60000.
    #[serde(default = "default_access_cache_ttl_ms")]
    pub access_cache_ttl_ms: u64,
    /// Default 10000.
    #[serde(default = "default_redaction_poll_interval_ms")]
    pub redaction_poll_interval_ms: u64,
    /// Default 600000.
    #[serde(default = "default_redaction_max_staleness_ms")]
    pub redaction_max_staleness_ms: u64,
    /// Default 256.
    #[serde(default = "default_upstream_max_concurrency")]
    pub upstream_max_concurrency: usize,
    /// (+) Default 4096.
    #[serde(default = "default_upstream_queue")]
    pub upstream_queue: usize,
    /// (+) Optional CA path for the upstream TLS connection.
    #[serde(default)]
    pub upstream_ca: Option<PathBuf>,
}

fn default_max_cache_bytes() -> u64 {
    50 * 1024 * 1024 * 1024
}
fn default_low_watermark_ratio() -> f64 {
    0.8
}
fn default_true() -> bool {
    true
}
fn default_ref_cache_ttl_ms() -> u64 {
    1000
}
fn default_access_cache_ttl_ms() -> u64 {
    60_000
}
fn default_redaction_poll_interval_ms() -> u64 {
    10_000
}
fn default_redaction_max_staleness_ms() -> u64 {
    600_000
}
fn default_upstream_max_concurrency() -> usize {
    256
}
fn default_upstream_queue() -> usize {
    4096
}

fn require(ok: bool, key: &str, msg: &str) -> Result<(), ConfigError> {
    if ok {
        Ok(())
    } else {
        Err(ConfigError::invalid(key, msg))
    }
}

impl DaemonConfig {
    /// Semantic checks beyond the schema (SPEC 6: invalid values are startup errors).
    fn validate(&self) -> Result<(), ConfigError> {
        require(!self.upstream.is_empty(), "upstream", "must not be empty")?;
        require(
            self.max_cache_bytes >= 1,
            "max_cache_bytes",
            "must be at least 1",
        )?;
        require(
            self.low_watermark_ratio > 0.0 && self.low_watermark_ratio <= 1.0,
            "low_watermark_ratio",
            "must be in (0, 1]",
        )?;
        require(
            self.redaction_poll_interval_ms >= 1,
            "redaction_poll_interval_ms",
            "must be at least 1",
        )?;
        require(
            self.redaction_max_staleness_ms >= self.redaction_poll_interval_ms,
            "redaction_max_staleness_ms",
            "must be at least redaction_poll_interval_ms",
        )?;
        require(
            self.upstream_max_concurrency >= 1,
            "upstream_max_concurrency",
            "must be at least 1",
        )?;
        Ok(())
    }
}

/// Parse daemon TOML text (SPEC 6.3): `$NAME` resolution, unknown-key rejection, SPEC 6.5
/// defaults.
pub fn parse_daemon(text: &str, env: &dyn Env) -> Result<DaemonConfig, ConfigError> {
    let cfg: DaemonConfig = decode(text, env)?;
    cfg.validate()?;
    Ok(cfg)
}

/// Read and parse a daemon config file (SPEC 6.3).
pub fn load_daemon(p: &Path, env: &dyn Env) -> Result<DaemonConfig, ConfigError> {
    parse_daemon(&read_file(p)?, env)
}

impl DaemonConfig {
    /// A complete config for in-process tests with paths under `dir` (SPEC 19.0): socket
    /// `dir/daemon.sock`, cache `dir/cache`, SPEC 6.5 defaults otherwise.
    pub fn for_tests(dir: &Path, upstream: &str) -> Self {
        DaemonConfig {
            socket: dir.join("daemon.sock"),
            upstream: upstream.to_string(),
            cache_dir: dir.join("cache"),
            max_cache_bytes: default_max_cache_bytes(),
            low_watermark_ratio: default_low_watermark_ratio(),
            verify_on_read: true,
            ref_cache_ttl_ms: default_ref_cache_ttl_ms(),
            access_cache_ttl_ms: default_access_cache_ttl_ms(),
            redaction_poll_interval_ms: default_redaction_poll_interval_ms(),
            redaction_max_staleness_ms: default_redaction_max_staleness_ms(),
            upstream_max_concurrency: default_upstream_max_concurrency(),
            upstream_queue: default_upstream_queue(),
            upstream_ca: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::MapEnv;

    #[test]
    fn for_tests_is_valid() {
        let c = DaemonConfig::for_tests(Path::new("/tmp/d"), "https://127.0.0.1:1");
        c.validate().unwrap();
        assert_eq!(c.socket, Path::new("/tmp/d/daemon.sock"));
        assert_eq!(c.upstream, "https://127.0.0.1:1");
    }

    #[test]
    fn validation_names_key() {
        let base = "socket = \"/s\"\nupstream = \"https://u\"\ncache_dir = \"/c\"\n";
        let env = MapEnv::default();
        parse_daemon(base, &env).unwrap();
        for (extra, key) in [
            ("low_watermark_ratio = 1.5\n", "low_watermark_ratio"),
            ("low_watermark_ratio = 0.0\n", "low_watermark_ratio"),
            ("upstream_max_concurrency = 0\n", "upstream_max_concurrency"),
            ("max_cache_bytes = \"big\"\n", "max_cache_bytes"),
            (
                "redaction_max_staleness_ms = 5\n",
                "redaction_max_staleness_ms",
            ),
        ] {
            let text = format!("{base}{extra}");
            assert!(
                matches!(parse_daemon(&text, &env), Err(ConfigError::Invalid { key: k, .. }) if k == key),
                "{extra}"
            );
        }
        assert!(matches!(
            parse_daemon("upstream = \"https://u\"\ncache_dir = \"/c\"\n", &env),
            Err(ConfigError::Invalid { key, .. }) if key == "socket"
        ));
    }
}
