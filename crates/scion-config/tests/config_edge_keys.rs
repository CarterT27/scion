//! SPEC 6 / 19.15 edge cases: unknown and invalid keys are named exactly, even when a key
//! repeats its section's name, sits in an inline table or in an array element.

use scion_config::{ConfigError, MapEnv, parse_daemon, parse_server};

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

fn empty() -> MapEnv {
    MapEnv::default()
}

fn unknown(key: &str) -> ConfigError {
    ConfigError::UnknownKey { key: key.into() }
}

fn invalid_key(r: Result<scion_config::ServerConfig, ConfigError>) -> String {
    match r {
        Err(ConfigError::Invalid { key, .. }) => key,
        other => format!("<expected Invalid, got {other:?}>"),
    }
}

#[test]
fn unknown_key_named_like_its_section() {
    let text = format!("{MINIMAL_SERVER}\n[limits]\nlimits = 5\n");
    assert_eq!(parse_server(&text, &empty()), Err(unknown("limits.limits")));
    let text = MINIMAL_SERVER.replace("repo = \"r\"", "repo = \"r\"\nserver = 1");
    assert_eq!(parse_server(&text, &empty()), Err(unknown("server.server")));
}

#[test]
fn unknown_key_in_inline_and_sub_tables() {
    let text = MINIMAL_SERVER.replace("repo = \"r\"", "repo = \"r\"\nextra = { a = 1 }");
    assert_eq!(parse_server(&text, &empty()), Err(unknown("server.extra")));
    let text = format!("{MINIMAL_SERVER}\n[limits.sub]\nx = 1\n");
    assert_eq!(parse_server(&text, &empty()), Err(unknown("limits.sub")));
    let text = format!("{MINIMAL_SERVER}\n[land]\nmax_attempts = 3\n[land.extra]\n");
    assert_eq!(parse_server(&text, &empty()), Err(unknown("land.extra")));
}

#[test]
fn wrong_type_names_key() {
    let text = MINIMAL_SERVER.replace("[\"/key\"]", "[1]");
    assert!(invalid_key(parse_server(&text, &empty())).starts_with("auth.signing_keys"));
    let text = format!("{MINIMAL_SERVER}\n[limits]\nmax_parents = -1\n");
    assert_eq!(
        invalid_key(parse_server(&text, &empty())),
        "limits.max_parents"
    );
    let text = format!("{MINIMAL_SERVER}\n[land]\nmax_attempts = \"many\"\n");
    assert_eq!(
        invalid_key(parse_server(&text, &empty())),
        "land.max_attempts"
    );
    let text = format!(
        "policy = 3\n{}",
        MINIMAL_SERVER.replace("[policy]\npath = \"/acl.toml\"", "")
    );
    assert_eq!(invalid_key(parse_server(&text, &empty())), "policy");
    // A string resolved from the environment is still type-checked against its key.
    let env = MapEnv::from_pairs([("N", "lots")]);
    let text = format!("{MINIMAL_SERVER}\n[derive]\nlazy_workers = \"$N\"\n");
    assert_eq!(
        invalid_key(parse_server(&text, &env)),
        "derive.lazy_workers"
    );
}

#[test]
fn durations_use_humantime() {
    let text = format!("{MINIMAL_SERVER}\n[redaction]\ndefault_quarantine = \"36h 30m\"\n",)
        .replace("signing_keys", "token_ttl = \"90m\"\nsigning_keys");
    let c = parse_server(&text, &empty()).unwrap();
    assert_eq!(c.auth.token_ttl.as_secs(), 90 * 60);
    assert_eq!(
        c.redaction.default_quarantine.as_secs(),
        36 * 3600 + 30 * 60
    );
    let text = MINIMAL_SERVER.replace("signing_keys", "token_ttl = 3600\nsigning_keys");
    assert_eq!(invalid_key(parse_server(&text, &empty())), "auth.token_ttl");
    let text = MINIMAL_SERVER.replace("signing_keys", "token_ttl = \"0s\"\nsigning_keys");
    assert_eq!(invalid_key(parse_server(&text, &empty())), "auth.token_ttl");
}

#[test]
fn secret_sha256_hex_is_normalized_to_lowercase() {
    let upper = "9F86D081884C7D659A2FEAA0C55AD015A3BF4F1B2B0B822CD15D6C15B0F00A08";
    let text = format!(
        "{MINIMAL_SERVER}\n[[auth.service_principals]]\nname = \"ci\"\nsecret_sha256 = \"{upper}\"\n"
    );
    let c = parse_server(&text, &empty()).unwrap();
    assert_eq!(
        c.auth.service_principals[0].secret_sha256,
        upper.to_ascii_lowercase()
    );
    let short = format!(
        "{MINIMAL_SERVER}\n[[auth.service_principals]]\nname = \"ci\"\nsecret_sha256 = \"abc\"\n"
    );
    assert_eq!(
        invalid_key(parse_server(&short, &empty())),
        "auth.service_principals[0].secret_sha256"
    );
    let dup = format!(
        "{MINIMAL_SERVER}\n[[auth.service_principals]]\nname = \"ci\"\nsecret_sha256 = \"{upper}\"\n[[auth.service_principals]]\nname = \"ci\"\nsecret_sha256 = \"{upper}\"\n"
    );
    assert_eq!(
        invalid_key(parse_server(&dup, &empty())),
        "auth.service_principals[1].name"
    );
}

#[test]
fn daemon_integer_ratio_and_ca() {
    let base = "socket = \"/s\"\nupstream = \"https://u\"\ncache_dir = \"/c\"\n";
    let d = parse_daemon(&format!("{base}low_watermark_ratio = 1\n"), &empty()).unwrap();
    assert_eq!(d.low_watermark_ratio, 1.0);
    let env = MapEnv::from_pairs([("CA", "/etc/ca.pem")]);
    let d = parse_daemon(&format!("{base}upstream_ca = \"$CA\"\n"), &env).unwrap();
    assert_eq!(
        d.upstream_ca.as_deref(),
        Some(std::path::Path::new("/etc/ca.pem"))
    );
}
