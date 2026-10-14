//! SPEC 19.15 Configuration: unknown keys rejected with the key name, `$VAR` resolution and
//! unresolved-variable errors, every SPEC 6.5 default (plus the PLAN 6.11 (+) keys) applied
//! when its key is omitted, and the SPEC 6.1 example file.

use scion_config::{
    ConfigError, MapEnv, load_daemon, load_server, parse_client, parse_daemon, parse_server,
    parse_wc,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// The SPEC 6.1 example, verbatim.
const SPEC_6_1_EXAMPLE: &str = r#"
[server]
listen = "0.0.0.0:7443"
tls_cert = "/etc/scion/tls.crt"
tls_key = "/etc/scion/tls.key"
repo = "monorepo"

[metadata]
url = "$SCION_DB_URL"
max_connections = 64

[objects]
endpoint = "https://s3.example.internal"
bucket = "scion"
prefix = "prod"
inline_threshold_bytes = 65536
zstd_level = 3

[auth]
github_client_id = "$SCION_GITHUB_CLIENT_ID"
github_api_url = "https://api.github.com"
token_ttl = "12h"
signing_keys = ["/etc/scion/keys/2026-10.ed25519"]   # first key signs; all keys verify

[[auth.service_principals]]
name = "ci"
secret_sha256 = "$SCION_CI_SECRET_SHA256"

[policy]
path = "/etc/scion/acl.toml"
reload = true

[limits]
max_blob_bytes = 104857600
max_delta_entries = 100000
max_parents = 16
max_message_bytes = 1048576
merge_max_text_bytes = 10485760

[land]
max_attempts = 32
backoff_base_ms = 5
backoff_max_ms = 500

[derive]
lease_ttl_ms = 30000
lazy_workers = 16

[redaction]
propagation_bound_ms = 1000
default_quarantine = "7d"

[observability]
log_format = "json"
metrics_listen = "127.0.0.1:9464"
"#;

/// The SPEC 6.3 example, verbatim.
const SPEC_6_3_EXAMPLE: &str = r#"
socket = "/run/scion/daemon.sock"
upstream = "https://scion.example.internal:7443"
cache_dir = "/var/cache/scion"
max_cache_bytes = 53687091200
low_watermark_ratio = 0.8
verify_on_read = true
ref_cache_ttl_ms = 1000
access_cache_ttl_ms = 60000
redaction_poll_interval_ms = 10000
redaction_max_staleness_ms = 600000
upstream_max_concurrency = 256
"#;

/// Only the required server keys; every defaultable key omitted.
const MINIMAL_SERVER: &str = r#"
[server]
listen = "127.0.0.1:7443"
tls_cert = "/c"
tls_key = "/k"
repo = "r"

[metadata]
url = "postgresql://db"

[objects]
endpoint = "https://s3"
bucket = "b"

[auth]
github_client_id = "id"
signing_keys = ["/key"]

[policy]
path = "/acl.toml"
"#;

const MINIMAL_DAEMON: &str = r#"
socket = "/run/scion/daemon.sock"
upstream = "https://scion.example.internal:7443"
cache_dir = "/var/cache/scion"
"#;

const SECRET_SHA: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

fn spec_env() -> MapEnv {
    MapEnv::from_pairs([
        ("SCION_DB_URL", "postgresql://root@db:26257/scion"),
        ("SCION_GITHUB_CLIENT_ID", "Iv1.abc"),
        ("SCION_CI_SECRET_SHA256", SECRET_SHA),
    ])
}

fn empty() -> MapEnv {
    MapEnv::default()
}

fn unknown(key: &str) -> ConfigError {
    ConfigError::UnknownKey { key: key.into() }
}

// ---- The SPEC 6.1 / 6.3 example files ------------------------------------------------------

#[test]
fn spec_6_1_example_parses_with_env() {
    let c = parse_server(SPEC_6_1_EXAMPLE, &spec_env()).unwrap();
    assert_eq!(
        c.server.listen,
        "0.0.0.0:7443".parse::<SocketAddr>().unwrap()
    );
    assert_eq!(c.server.tls_cert, PathBuf::from("/etc/scion/tls.crt"));
    assert_eq!(c.server.tls_key, PathBuf::from("/etc/scion/tls.key"));
    assert_eq!(c.server.repo, "monorepo");
    assert_eq!(c.metadata.url, "postgresql://root@db:26257/scion");
    assert_eq!(c.metadata.max_connections, 64);
    assert_eq!(c.objects.endpoint, "https://s3.example.internal");
    assert_eq!(c.objects.bucket, "scion");
    assert_eq!(c.objects.prefix, "prod");
    assert_eq!(c.objects.inline_threshold_bytes, 65536);
    assert_eq!(c.objects.zstd_level, 3);
    assert_eq!(c.auth.github_client_id, "Iv1.abc");
    assert_eq!(c.auth.github_api_url, "https://api.github.com");
    assert_eq!(c.auth.token_ttl, Duration::from_secs(12 * 3600));
    assert_eq!(
        c.auth.signing_keys,
        vec![PathBuf::from("/etc/scion/keys/2026-10.ed25519")]
    );
    assert_eq!(c.auth.service_principals.len(), 1);
    assert_eq!(c.auth.service_principals[0].name, "ci");
    assert_eq!(c.auth.service_principals[0].secret_sha256, SECRET_SHA);
    assert_eq!(c.policy.path, PathBuf::from("/etc/scion/acl.toml"));
    assert!(c.policy.reload);
    assert_eq!(c.limits, scion_core::Limits::default());
    assert_eq!(c.land.max_attempts, 32);
    assert_eq!(c.land.backoff_base_ms, 5);
    assert_eq!(c.land.backoff_max_ms, 500);
    assert_eq!(c.derive.lease_ttl_ms, 30000);
    assert_eq!(c.derive.lazy_workers, 16);
    assert_eq!(c.redaction.propagation_bound_ms, 1000);
    assert_eq!(
        c.redaction.default_quarantine,
        Duration::from_secs(7 * 86400)
    );
    assert_eq!(c.observability.log_format, "json");
    assert_eq!(
        c.observability.metrics_listen,
        "127.0.0.1:9464".parse::<SocketAddr>().unwrap()
    );
    // (+) keys absent from the example take their defaults.
    assert_eq!(c.objects.region, "us-east-1");
    assert!(c.objects.path_style);
    assert_eq!(c.auth.github_oauth_base_url, "https://github.com");
    assert_eq!(c.rate_limit.per_principal_rps, 500);
}

#[test]
fn spec_6_3_example_parses() {
    let d = parse_daemon(SPEC_6_3_EXAMPLE, &empty()).unwrap();
    assert_eq!(d.socket, PathBuf::from("/run/scion/daemon.sock"));
    assert_eq!(d.upstream, "https://scion.example.internal:7443");
    assert_eq!(d.cache_dir, PathBuf::from("/var/cache/scion"));
    assert_eq!(d.max_cache_bytes, 53687091200);
    assert_eq!(d.low_watermark_ratio, 0.8);
    assert!(d.verify_on_read);
    assert_eq!(d.upstream_max_concurrency, 256);
    assert_eq!(d.upstream_queue, 4096);
    assert_eq!(d.upstream_ca, None);
}

#[test]
fn load_from_files() {
    let dir = tempfile::tempdir().unwrap();
    let sp = dir.path().join("scion-server.toml");
    std::fs::write(&sp, SPEC_6_1_EXAMPLE).unwrap();
    assert_eq!(
        load_server(&sp, &spec_env()).unwrap(),
        parse_server(SPEC_6_1_EXAMPLE, &spec_env()).unwrap()
    );
    let dp = dir.path().join("scion-daemon.toml");
    std::fs::write(&dp, SPEC_6_3_EXAMPLE).unwrap();
    assert_eq!(
        load_daemon(&dp, &empty()).unwrap(),
        parse_daemon(SPEC_6_3_EXAMPLE, &empty()).unwrap()
    );
    let missing = dir.path().join("missing.toml");
    assert!(matches!(
        load_server(&missing, &empty()),
        Err(ConfigError::Io(_))
    ));
    assert!(matches!(
        load_daemon(&missing, &empty()),
        Err(ConfigError::Io(_))
    ));
}

#[test]
fn syntax_error_is_parse_error() {
    assert!(matches!(
        parse_server("[server\nlisten=", &empty()),
        Err(ConfigError::Parse(_))
    ));
    assert!(matches!(
        parse_daemon("socket = ", &empty()),
        Err(ConfigError::Parse(_))
    ));
    assert!(matches!(
        parse_client("endpoint = = 1", &empty()),
        Err(ConfigError::Parse(_))
    ));
}

// ---- Unknown keys -----------------------------------------------------------------------------

#[test]
fn server_unknown_key_top_level() {
    let text = format!("bogus = 1\n{MINIMAL_SERVER}");
    assert_eq!(parse_server(&text, &empty()), Err(unknown("bogus")));
    let text = format!("{MINIMAL_SERVER}\n[bogus_section]\nx = 1\n");
    assert_eq!(parse_server(&text, &empty()), Err(unknown("bogus_section")));
}

#[test]
fn server_unknown_key_nested() {
    let text = format!("{MINIMAL_SERVER}\n[limits]\nmax_blobs = 1\n");
    assert_eq!(
        parse_server(&text, &empty()),
        Err(unknown("limits.max_blobs"))
    );
    let text = MINIMAL_SERVER.replace("repo = \"r\"", "repo = \"r\"\nrepository = \"x\"");
    assert_eq!(
        parse_server(&text, &empty()),
        Err(unknown("server.repository"))
    );
    let text = format!("{MINIMAL_SERVER}\n[rate_limit]\nrps = 1\n");
    assert_eq!(
        parse_server(&text, &empty()),
        Err(unknown("rate_limit.rps"))
    );
    let text = format!("{MINIMAL_SERVER}\n[redaction]\nquarantine = \"1d\"\n");
    assert_eq!(
        parse_server(&text, &empty()),
        Err(unknown("redaction.quarantine"))
    );
    let text = format!(
        "{MINIMAL_SERVER}\n[[auth.service_principals]]\nname = \"ci\"\nsecret_sha256 = \"{SECRET_SHA}\"\nsecret = \"x\"\n"
    );
    assert_eq!(
        parse_server(&text, &empty()),
        Err(unknown("auth.service_principals[0].secret"))
    );
}

#[test]
fn daemon_unknown_key() {
    let text = format!("{MINIMAL_DAEMON}max_cache = 1\n");
    assert_eq!(parse_daemon(&text, &empty()), Err(unknown("max_cache")));
    let text = format!("{MINIMAL_DAEMON}\n[cache]\nmax_bytes = 1\n");
    assert_eq!(parse_daemon(&text, &empty()), Err(unknown("cache")));
}

#[test]
fn client_and_wc_unknown_key() {
    assert_eq!(
        parse_client("endpoint = \"https://x\"\nendpiont = \"y\"\n", &empty()),
        Err(unknown("endpiont"))
    );
    assert_eq!(
        parse_client("[auth]\ntoken = \"t\"\n", &empty()),
        Err(unknown("auth"))
    );
    assert_eq!(
        parse_wc("tracking_ref = \"main\"\ntracking = \"x\"\n", &empty()),
        Err(unknown("tracking"))
    );
}

#[test]
fn unknown_key_error_message_names_key() {
    let text = format!("{MINIMAL_SERVER}\n[limits]\nmax_blobs = 1\n");
    let err = parse_server(&text, &empty()).unwrap_err();
    assert!(err.to_string().contains("limits.max_blobs"), "{err}");
}

// ---- $VAR resolution --------------------------------------------------------------------------

#[test]
fn var_resolves_in_every_config() {
    let env = MapEnv::from_pairs([
        ("REPO", "monorepo"),
        ("KEY", "/keys/a.pem"),
        ("SOCK", "/tmp/s.sock"),
        ("EP", "unix:///tmp/s.sock"),
        ("LISTEN", "127.0.0.1:9000"),
    ]);
    let text = MINIMAL_SERVER
        .replace("repo = \"r\"", "repo = \"$REPO\"")
        .replace("[\"/key\"]", "[\"$KEY\"]")
        .replace("\"127.0.0.1:7443\"", "\"$LISTEN\"");
    let c = parse_server(&text, &env).unwrap();
    assert_eq!(c.server.repo, "monorepo");
    assert_eq!(c.auth.signing_keys, vec![PathBuf::from("/keys/a.pem")]);
    assert_eq!(c.server.listen.port(), 9000);

    let d = parse_daemon(
        &MINIMAL_DAEMON.replace("\"/run/scion/daemon.sock\"", "\"$SOCK\""),
        &env,
    )
    .unwrap();
    assert_eq!(d.socket, PathBuf::from("/tmp/s.sock"));

    let u = parse_client("endpoint = \"$EP\"\n", &env).unwrap();
    assert_eq!(u.endpoint.as_deref(), Some("unix:///tmp/s.sock"));
    let w = parse_wc("endpoint = \"$EP\"\ntracking_ref = \"main\"\n", &env).unwrap();
    assert_eq!(w.endpoint.as_deref(), Some("unix:///tmp/s.sock"));
    assert_eq!(w.tracking_ref.as_deref(), Some("main"));
}

#[test]
fn non_variable_dollar_strings_are_literal() {
    let text = MINIMAL_SERVER.replace("repo = \"r\"", "repo = \"price$5\"");
    assert_eq!(
        parse_server(&text, &empty()).unwrap().server.repo,
        "price$5"
    );
    let u = parse_client("endpoint = \"$1\"\n", &empty()).unwrap();
    assert_eq!(u.endpoint.as_deref(), Some("$1"));
}

#[test]
fn unresolved_var_names_variable_and_key() {
    let mut env = spec_env();
    env.0.remove("SCION_DB_URL");
    assert_eq!(
        parse_server(SPEC_6_1_EXAMPLE, &env),
        Err(ConfigError::UnresolvedVar {
            name: "SCION_DB_URL".into(),
            key: "metadata.url".into()
        })
    );
    let mut env = spec_env();
    env.0.remove("SCION_CI_SECRET_SHA256");
    let err = parse_server(SPEC_6_1_EXAMPLE, &env).unwrap_err();
    assert_eq!(
        err,
        ConfigError::UnresolvedVar {
            name: "SCION_CI_SECRET_SHA256".into(),
            key: "auth.service_principals[0].secret_sha256".into()
        }
    );
    let msg = err.to_string();
    assert!(msg.contains("SCION_CI_SECRET_SHA256") && msg.contains("secret_sha256"));

    assert_eq!(
        parse_daemon(
            &format!("{MINIMAL_DAEMON}upstream_ca = \"$CA\"\n"),
            &empty()
        ),
        Err(ConfigError::UnresolvedVar {
            name: "CA".into(),
            key: "upstream_ca".into()
        })
    );
    assert_eq!(
        parse_client("endpoint = \"$NOPE\"\n", &empty()),
        Err(ConfigError::UnresolvedVar {
            name: "NOPE".into(),
            key: "endpoint".into()
        })
    );
    assert_eq!(
        parse_wc("tracking_ref = \"$NOPE\"\n", &empty()),
        Err(ConfigError::UnresolvedVar {
            name: "NOPE".into(),
            key: "tracking_ref".into()
        })
    );
}

// ---- SPEC 6.5 defaults: one assertion per row, plus the (+) keys -----------------------------

#[test]
fn spec_6_5_server_defaults() {
    let c = parse_server(MINIMAL_SERVER, &empty()).unwrap();
    // objects.inline_threshold_bytes = 65536
    assert_eq!(c.objects.inline_threshold_bytes, 65536);
    // objects.zstd_level = 3
    assert_eq!(c.objects.zstd_level, 3);
    // auth.token_ttl = 12h
    assert_eq!(c.auth.token_ttl, Duration::from_secs(12 * 60 * 60));
    // limits.max_blob_bytes = 100 MiB
    assert_eq!(c.limits.max_blob_bytes, 100 * 1024 * 1024);
    // limits.max_delta_entries = 100000
    assert_eq!(c.limits.max_delta_entries, 100_000);
    // limits.max_parents = 16
    assert_eq!(c.limits.max_parents, 16);
    // limits.merge_max_text_bytes = 10 MiB
    assert_eq!(c.limits.merge_max_text_bytes, 10 * 1024 * 1024);
    // land.max_attempts = 32
    assert_eq!(c.land.max_attempts, 32);
    // redaction.propagation_bound_ms = 1000
    assert_eq!(c.redaction.propagation_bound_ms, 1000);
    // redaction.default_quarantine = 7d
    assert_eq!(
        c.redaction.default_quarantine,
        Duration::from_secs(7 * 24 * 60 * 60)
    );
}

#[test]
fn plan_2_11_server_defaults() {
    let c = parse_server(MINIMAL_SERVER, &empty()).unwrap();
    assert_eq!(c.metadata.max_connections, 64);
    assert_eq!(c.objects.prefix, "");
    assert_eq!(c.limits.max_message_bytes, 1_048_576);
    assert_eq!(c.land.backoff_base_ms, 5);
    assert_eq!(c.land.backoff_max_ms, 500);
    assert_eq!(c.derive.lease_ttl_ms, 30_000);
    assert_eq!(c.derive.lazy_workers, 16);
    assert_eq!(c.auth.github_api_url, "https://api.github.com");
    assert!(c.auth.service_principals.is_empty());
    assert!(c.policy.reload);
    assert_eq!(c.observability.log_format, "json");
    assert_eq!(
        c.observability.metrics_listen,
        "127.0.0.1:9464".parse::<SocketAddr>().unwrap()
    );
}

#[test]
fn plan_6_11_plus_key_defaults() {
    let c = parse_server(MINIMAL_SERVER, &empty()).unwrap();
    // [rate_limit] (+)
    assert_eq!(c.rate_limit.per_principal_rps, 500);
    assert_eq!(c.rate_limit.burst, 1000);
    assert_eq!(c.rate_limit.max_inflight, 4096);
    // [objects] (+)
    assert_eq!(c.objects.region, "us-east-1");
    assert!(c.objects.path_style);
    // [auth] (+)
    assert_eq!(c.auth.github_oauth_base_url, "https://github.com");
    // daemon (+)
    let d = parse_daemon(MINIMAL_DAEMON, &empty()).unwrap();
    assert_eq!(d.upstream_queue, 4096);
    assert_eq!(d.upstream_ca, None);
}

#[test]
fn spec_6_5_daemon_defaults() {
    let d = parse_daemon(MINIMAL_DAEMON, &empty()).unwrap();
    // daemon.max_cache_bytes = 50 GiB
    assert_eq!(d.max_cache_bytes, 50 * 1024 * 1024 * 1024);
    // daemon.ref_cache_ttl_ms = 1000
    assert_eq!(d.ref_cache_ttl_ms, 1000);
    // daemon.access_cache_ttl_ms = 60000
    assert_eq!(d.access_cache_ttl_ms, 60_000);
    // daemon.redaction_poll_interval_ms = 10000
    assert_eq!(d.redaction_poll_interval_ms, 10_000);
    // daemon.redaction_max_staleness_ms = 600000
    assert_eq!(d.redaction_max_staleness_ms, 600_000);
    // SPEC 6.3 example values used as defaults (PLAN 2.11).
    assert_eq!(d.low_watermark_ratio, 0.8);
    assert!(d.verify_on_read);
    assert_eq!(d.upstream_max_concurrency, 256);
}

#[test]
fn partial_sections_keep_other_defaults() {
    let text = format!(
        "{MINIMAL_SERVER}\n[limits]\nmax_parents = 4\n[land]\nmax_attempts = 8\n[redaction]\ndefault_quarantine = \"36h\"\n"
    );
    let c = parse_server(&text, &empty()).unwrap();
    assert_eq!(c.limits.max_parents, 4);
    assert_eq!(c.limits.max_blob_bytes, 100 * 1024 * 1024);
    assert_eq!(c.land.max_attempts, 8);
    assert_eq!(c.land.backoff_max_ms, 500);
    assert_eq!(
        c.redaction.default_quarantine,
        Duration::from_secs(36 * 3600)
    );
    assert_eq!(c.redaction.propagation_bound_ms, 1000);
}

#[test]
fn client_defaults() {
    assert_eq!(parse_client("", &empty()).unwrap().endpoint, None);
    let w = parse_wc("", &empty()).unwrap();
    assert_eq!(w.endpoint, None);
    assert_eq!(w.tracking_ref, None);
}

#[test]
fn missing_required_key_is_named() {
    let text = MINIMAL_SERVER.replace("bucket = \"b\"\n", "");
    assert_eq!(
        parse_server(&text, &empty()),
        Err(ConfigError::Invalid {
            key: "objects.bucket".into(),
            msg: "required key is missing".into()
        })
    );
    let text = MINIMAL_SERVER.replace("[policy]\npath = \"/acl.toml\"\n", "");
    assert!(matches!(
        parse_server(&text, &empty()),
        Err(ConfigError::Invalid { key, .. }) if key == "policy"
    ));
}

#[test]
fn for_tests_constructors() {
    let dir = tempfile::tempdir().unwrap();
    let s = scion_config::ServerConfig::for_tests(dir.path());
    assert!(s.server.tls_cert.starts_with(dir.path()));
    assert!(s.policy.path.starts_with(dir.path()));
    assert_eq!(s.limits, scion_core::Limits::default());
    let d = scion_config::DaemonConfig::for_tests(dir.path(), "https://127.0.0.1:7443");
    assert!(d.socket.starts_with(dir.path()));
    assert!(d.cache_dir.starts_with(dir.path()));
    assert_eq!(d.upstream, "https://127.0.0.1:7443");
}
