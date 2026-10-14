//! `scion-server.toml` (SPEC 6.1 plus the PLAN 6.11 keys marked (+); P04).
//!
//! Key policy: every key with a SPEC 6.5 / PLAN 2.11 / PLAN 6.11 default (and the obviously
//! defaultable keys `objects.prefix`, `auth.github_api_url`, `auth.service_principals`,
//! `policy.reload`, `observability.*`) may be omitted. Deployment-identity keys
//! (`server.*`, `metadata.url`, `objects.endpoint`, `objects.bucket`,
//! `auth.github_client_id`, `auth.signing_keys`, `policy.path`) are required. The sections
//! `[limits]`, `[land]`, `[derive]`, `[redaction]`, `[observability]` and `[rate_limit]` may be
//! omitted entirely. Unknown keys are rejected everywhere (SPEC 6).

use crate::env::{Env, decode, read_file};
use crate::error::ConfigError;
use scion_core::Limits;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The server configuration: sections exactly as SPEC 6.1 plus `[rate_limit]` (+).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
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
    #[serde(default)]
    pub limits: Limits,
    /// `[land]`.
    #[serde(default)]
    pub land: LandSection,
    /// `[derive]`.
    #[serde(default)]
    pub derive: DeriveSection,
    /// `[redaction]`.
    #[serde(default)]
    pub redaction: RedactionSection,
    /// `[observability]`.
    #[serde(default)]
    pub observability: ObservabilitySection,
    /// `[rate_limit]` (+).
    #[serde(default)]
    pub rate_limit: RateLimitSection,
}

/// `[server]` (SPEC 6.1). All keys required.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// `[metadata]` (SPEC 6.1).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataSection {
    /// CockroachDB URL.
    pub url: String,
    /// Pool size (default 64).
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
}

/// `[objects]` (SPEC 6.1, 6.5; PLAN 6.11).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectsSection {
    /// S3 endpoint.
    pub endpoint: String,
    /// Bucket.
    pub bucket: String,
    /// Key prefix (default empty).
    #[serde(default)]
    pub prefix: String,
    /// Inline threshold (default 65536).
    #[serde(default = "default_inline_threshold")]
    pub inline_threshold_bytes: usize,
    /// zstd level (default 3).
    #[serde(default = "default_zstd_level")]
    pub zstd_level: i32,
    /// (+) Region (default `us-east-1`).
    #[serde(default = "default_region")]
    pub region: String,
    /// (+) Path-style addressing (default true).
    #[serde(default = "default_true")]
    pub path_style: bool,
}

/// `[auth]` (SPEC 6.1, 6.5; PLAN 6.1, 6.11).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthSection {
    /// GitHub OAuth App client id.
    pub github_client_id: String,
    /// GitHub REST API base URL (default `https://api.github.com`).
    #[serde(default = "default_github_api_url")]
    pub github_api_url: String,
    /// (+) GitHub OAuth base URL (default `https://github.com`).
    #[serde(default = "default_github_oauth_base_url")]
    pub github_oauth_base_url: String,
    /// Token TTL (default 12h), humantime syntax.
    #[serde(default = "default_token_ttl", with = "humantime_serde")]
    pub token_ttl: Duration,
    /// Signing key PEM paths; the first signs, all verify. At least one is required.
    pub signing_keys: Vec<PathBuf>,
    /// `[[auth.service_principals]]` (default none).
    #[serde(default)]
    pub service_principals: Vec<ServicePrincipalConfig>,
}

/// `[[auth.service_principals]]` (SPEC 6.1, 12).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServicePrincipalConfig {
    /// Service name.
    pub name: String,
    /// SHA-256 of the secret as 64 hex characters (normalized to lowercase by
    /// [`parse_server`]).
    pub secret_sha256: String,
}

/// `[policy]` (SPEC 6.1, 6.2).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySection {
    /// `acl.toml` path.
    pub path: PathBuf,
    /// Watch and hot-reload (default true).
    #[serde(default = "default_true")]
    pub reload: bool,
}

/// `[land]` (SPEC 6.1, 6.5).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LandSection {
    /// Default 32.
    pub max_attempts: u32,
    /// Default 5.
    pub backoff_base_ms: u64,
    /// Default 500.
    pub backoff_max_ms: u64,
}

impl Default for LandSection {
    /// SPEC 6.1 / 6.5 defaults.
    fn default() -> Self {
        LandSection {
            max_attempts: 32,
            backoff_base_ms: 5,
            backoff_max_ms: 500,
        }
    }
}

/// `[derive]` (SPEC 6.1).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeriveSection {
    /// Default 30000.
    pub lease_ttl_ms: u64,
    /// Default 16.
    pub lazy_workers: usize,
}

impl Default for DeriveSection {
    /// SPEC 6.1 defaults.
    fn default() -> Self {
        DeriveSection {
            lease_ttl_ms: 30_000,
            lazy_workers: 16,
        }
    }
}

/// `[redaction]` (SPEC 6.1, 6.5).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RedactionSection {
    /// Default 1000.
    pub propagation_bound_ms: u64,
    /// Default 7d, humantime syntax.
    #[serde(with = "humantime_serde")]
    pub default_quarantine: Duration,
}

impl Default for RedactionSection {
    /// SPEC 6.5 defaults.
    fn default() -> Self {
        RedactionSection {
            propagation_bound_ms: 1000,
            default_quarantine: Duration::from_secs(7 * 24 * 3600),
        }
    }
}

/// `[observability]` (SPEC 6.1, 15).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObservabilitySection {
    /// `json` (default, SPEC 15.1) or `text`.
    pub log_format: String,
    /// Admin listener (metrics/health; default `127.0.0.1:9464`).
    pub metrics_listen: SocketAddr,
}

impl Default for ObservabilitySection {
    /// SPEC 6.1 / 15.1 defaults.
    fn default() -> Self {
        ObservabilitySection {
            log_format: "json".to_string(),
            metrics_listen: SocketAddr::from(([127, 0, 0, 1], 9464)),
        }
    }
}

/// `[rate_limit]` (+) (PLAN 6.11; SPEC 10.5).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimitSection {
    /// Default 500.
    pub per_principal_rps: u32,
    /// Default 1000.
    pub burst: u32,
    /// Default 4096.
    pub max_inflight: usize,
}

impl Default for RateLimitSection {
    /// PLAN 6.11 defaults.
    fn default() -> Self {
        RateLimitSection {
            per_principal_rps: 500,
            burst: 1000,
            max_inflight: 4096,
        }
    }
}

fn default_max_connections() -> usize {
    64
}
fn default_inline_threshold() -> usize {
    65_536
}
fn default_zstd_level() -> i32 {
    3
}
fn default_region() -> String {
    "us-east-1".to_string()
}
fn default_true() -> bool {
    true
}
fn default_github_api_url() -> String {
    "https://api.github.com".to_string()
}
fn default_github_oauth_base_url() -> String {
    "https://github.com".to_string()
}
fn default_token_ttl() -> Duration {
    Duration::from_secs(12 * 3600)
}

fn require(ok: bool, key: &str, msg: &str) -> Result<(), ConfigError> {
    if ok {
        Ok(())
    } else {
        Err(ConfigError::invalid(key, msg))
    }
}

fn is_lower_hex_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl ServerConfig {
    /// Semantic checks beyond the schema (SPEC 6: invalid values are startup errors).
    fn validate(&self) -> Result<(), ConfigError> {
        require(
            !self.server.repo.is_empty(),
            "server.repo",
            "must not be empty",
        )?;
        require(
            !self.metadata.url.is_empty(),
            "metadata.url",
            "must not be empty",
        )?;
        require(
            self.metadata.max_connections >= 1,
            "metadata.max_connections",
            "must be at least 1",
        )?;
        require(
            !self.objects.bucket.is_empty(),
            "objects.bucket",
            "must not be empty",
        )?;
        require(
            (-131_072..=22).contains(&self.objects.zstd_level),
            "objects.zstd_level",
            "must be a zstd level (at most 22)",
        )?;
        require(
            !self.auth.signing_keys.is_empty(),
            "auth.signing_keys",
            "at least one signing key is required",
        )?;
        require(
            !self.auth.token_ttl.is_zero(),
            "auth.token_ttl",
            "must be positive",
        )?;
        let mut names = std::collections::BTreeSet::new();
        for (i, sp) in self.auth.service_principals.iter().enumerate() {
            let key = format!("auth.service_principals[{i}]");
            require(
                !sp.name.is_empty(),
                &format!("{key}.name"),
                "must not be empty",
            )?;
            require(
                names.insert(sp.name.as_str()),
                &format!("{key}.name"),
                "duplicate service principal name",
            )?;
            require(
                is_lower_hex_sha256(&sp.secret_sha256),
                &format!("{key}.secret_sha256"),
                "must be 64 lowercase hex characters",
            )?;
        }
        let l = &self.limits;
        for (name, v) in [
            ("limits.max_blob_bytes", l.max_blob_bytes),
            ("limits.max_delta_entries", l.max_delta_entries),
            ("limits.max_parents", l.max_parents),
            ("limits.max_message_bytes", l.max_message_bytes),
        ] {
            require(v >= 1, name, "must be at least 1")?;
        }
        require(
            self.land.max_attempts >= 1,
            "land.max_attempts",
            "must be at least 1",
        )?;
        require(
            self.land.backoff_base_ms <= self.land.backoff_max_ms,
            "land.backoff_base_ms",
            "must not exceed land.backoff_max_ms",
        )?;
        require(
            self.derive.lease_ttl_ms >= 1,
            "derive.lease_ttl_ms",
            "must be at least 1",
        )?;
        require(
            matches!(self.observability.log_format.as_str(), "json" | "text"),
            "observability.log_format",
            "must be \"json\" or \"text\"",
        )?;
        require(
            self.rate_limit.per_principal_rps >= 1,
            "rate_limit.per_principal_rps",
            "must be at least 1",
        )?;
        require(
            self.rate_limit.burst >= 1,
            "rate_limit.burst",
            "must be at least 1",
        )?;
        require(
            self.rate_limit.max_inflight >= 1,
            "rate_limit.max_inflight",
            "must be at least 1",
        )?;
        Ok(())
    }
}

/// Parse server TOML text (SPEC 6.1): `$NAME` resolution, unknown-key rejection naming the
/// full key, SPEC 6.5 defaults.
///
/// `auth.service_principals[].secret_sha256` accepts hex in either case and is normalized to
/// lowercase.
pub fn parse_server(text: &str, env: &dyn Env) -> Result<ServerConfig, ConfigError> {
    let mut cfg: ServerConfig = decode(text, env)?;
    for sp in &mut cfg.auth.service_principals {
        sp.secret_sha256.make_ascii_lowercase();
    }
    cfg.validate()?;
    Ok(cfg)
}

/// Read and parse a server config file (SPEC 6.1).
pub fn load_server(p: &Path, env: &dyn Env) -> Result<ServerConfig, ConfigError> {
    parse_server(&read_file(p)?, env)
}

impl ServerConfig {
    /// A complete config for in-process tests with paths under `dir` (SPEC 19.0). Listeners
    /// bind `127.0.0.1:0`; storage URLs are `mem://`; everything else takes its default.
    pub fn for_tests(dir: &Path) -> Self {
        let any_port = SocketAddr::from(([127, 0, 0, 1], 0));
        ServerConfig {
            server: ServerSection {
                listen: any_port,
                tls_cert: dir.join("tls.crt"),
                tls_key: dir.join("tls.key"),
                repo: "test".to_string(),
            },
            metadata: MetadataSection {
                url: "mem://".to_string(),
                max_connections: default_max_connections(),
            },
            objects: ObjectsSection {
                endpoint: "mem://".to_string(),
                bucket: "scion-test".to_string(),
                prefix: "test".to_string(),
                inline_threshold_bytes: default_inline_threshold(),
                zstd_level: default_zstd_level(),
                region: default_region(),
                path_style: true,
            },
            auth: AuthSection {
                github_client_id: "scion-test-client".to_string(),
                github_api_url: default_github_api_url(),
                github_oauth_base_url: default_github_oauth_base_url(),
                token_ttl: default_token_ttl(),
                signing_keys: vec![dir.join("signing.ed25519.pem")],
                service_principals: Vec::new(),
            },
            policy: PolicySection {
                path: dir.join("acl.toml"),
                reload: false,
            },
            limits: Limits::default(),
            land: LandSection::default(),
            derive: DeriveSection::default(),
            redaction: RedactionSection::default(),
            observability: ObservabilitySection {
                log_format: "json".to_string(),
                metrics_listen: any_port,
            },
            rate_limit: RateLimitSection::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::MapEnv;

    #[test]
    fn for_tests_is_valid() {
        let dir = Path::new("/tmp/scion-cfg-test");
        let c = ServerConfig::for_tests(dir);
        c.validate().unwrap();
        assert!(c.server.tls_cert.starts_with(dir));
        assert_eq!(c.server.listen.port(), 0);
    }

    #[test]
    fn validation_names_key() {
        let base = r#"
[server]
listen = "127.0.0.1:1"
tls_cert = "c"
tls_key = "k"
repo = "r"
[metadata]
url = "u"
[objects]
endpoint = "e"
bucket = "b"
[auth]
github_client_id = "x"
signing_keys = ["k"]
[policy]
path = "p"
"#;
        let env = MapEnv::default();
        parse_server(base, &env).unwrap();
        let bad = format!("{base}[observability]\nlog_format = \"xml\"\n");
        assert!(matches!(
            parse_server(&bad, &env),
            Err(ConfigError::Invalid { key, .. }) if key == "observability.log_format"
        ));
        let bad = base.replace("signing_keys = [\"k\"]", "signing_keys = []");
        assert!(matches!(
            parse_server(&bad, &env),
            Err(ConfigError::Invalid { key, .. }) if key == "auth.signing_keys"
        ));
        let bad = format!("{base}[land]\nbackoff_base_ms = 900\n");
        assert!(matches!(
            parse_server(&bad, &env),
            Err(ConfigError::Invalid { key, .. }) if key == "land.backoff_base_ms"
        ));
        let bad = base.replace("listen = \"127.0.0.1:1\"", "listen = \"nope\"");
        assert!(matches!(
            parse_server(&bad, &env),
            Err(ConfigError::Invalid { key, .. }) if key == "server.listen"
        ));
        let bad = base.replace("url = \"u\"\n", "");
        assert!(matches!(
            parse_server(&bad, &env),
            Err(ConfigError::Invalid { key, .. }) if key == "metadata.url"
        ));
        let bad =
            format!("{base}[[auth.service_principals]]\nname = \"ci\"\nsecret_sha256 = \"XYZ\"\n");
        // A `[[...]]` after `[policy]` re-opens `auth`; TOML allows array-of-tables there.
        assert!(matches!(
            parse_server(&bad, &env),
            Err(ConfigError::Invalid { key, .. }) if key == "auth.service_principals[0].secret_sha256"
        ));
        let bad = format!("{base}[redaction]\ndefault_quarantine = \"soon\"\n");
        assert!(matches!(
            parse_server(&bad, &env),
            Err(ConfigError::Invalid { key, .. }) if key == "redaction.default_quarantine"
        ));
    }
}
