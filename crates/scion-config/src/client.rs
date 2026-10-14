//! Client and working-copy configuration (SPEC 6.4, 11.1; PLAN 6.15; P04).

use crate::env::{Env, decode};
use crate::error::ConfigError;
use serde::{Deserialize, Serialize};

/// The environment variable overriding every configured endpoint (SPEC 6.4).
pub const SCION_ENDPOINT: &str = "SCION_ENDPOINT";

/// `~/.config/scion/config.toml` (SPEC 6.4).
#[derive(Clone, Debug, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientConfig {
    /// `unix:///...` or `https://...`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// `<root>/.scion/config.toml` (SPEC 11.1; PLAN 6.15).
#[derive(Clone, Debug, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WcConfig {
    /// Endpoint override for this working copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Tracked ref.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracking_ref: Option<String>,
}

/// Parse the user config (SPEC 6.4): `$NAME` resolution and unknown-key rejection.
pub fn parse_client(text: &str, env: &dyn Env) -> Result<ClientConfig, ConfigError> {
    decode(text, env)
}

/// Parse a working-copy config (SPEC 11.1; PLAN 6.15): `$NAME` resolution and unknown-key
/// rejection.
pub fn parse_wc(text: &str, env: &dyn Env) -> Result<WcConfig, ConfigError> {
    decode(text, env)
}

fn non_empty(s: Option<&str>) -> Option<String> {
    s.filter(|s| !s.is_empty()).map(str::to_string)
}

/// Endpoint precedence (SPEC 6.4): flag > `SCION_ENDPOINT` > working-copy config > user
/// config. Empty values count as unset.
pub fn resolve_endpoint(
    flag: Option<&str>,
    env: &dyn Env,
    wc: Option<&WcConfig>,
    user: Option<&ClientConfig>,
) -> Option<String> {
    non_empty(flag)
        .or_else(|| non_empty(env.var(SCION_ENDPOINT).as_deref()))
        .or_else(|| non_empty(wc.and_then(|w| w.endpoint.as_deref())))
        .or_else(|| non_empty(user.and_then(|u| u.endpoint.as_deref())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::MapEnv;

    #[test]
    fn precedence() {
        let wc = WcConfig {
            endpoint: Some("wc".into()),
            tracking_ref: None,
        };
        let user = ClientConfig {
            endpoint: Some("user".into()),
        };
        let env = MapEnv::from_pairs([(SCION_ENDPOINT, "env")]);
        let none = MapEnv::default();
        assert_eq!(
            resolve_endpoint(Some("flag"), &env, Some(&wc), Some(&user)).as_deref(),
            Some("flag")
        );
        assert_eq!(
            resolve_endpoint(None, &env, Some(&wc), Some(&user)).as_deref(),
            Some("env")
        );
        assert_eq!(
            resolve_endpoint(None, &none, Some(&wc), Some(&user)).as_deref(),
            Some("wc")
        );
        assert_eq!(
            resolve_endpoint(None, &none, None, Some(&user)).as_deref(),
            Some("user")
        );
        assert_eq!(
            resolve_endpoint(None, &none, Some(&WcConfig::default()), Some(&user)).as_deref(),
            Some("user")
        );
        assert_eq!(resolve_endpoint(None, &none, None, None), None);
        let empty_env = MapEnv::from_pairs([(SCION_ENDPOINT, "")]);
        assert_eq!(
            resolve_endpoint(Some(""), &empty_env, None, Some(&user)).as_deref(),
            Some("user")
        );
    }

    #[test]
    fn wc_round_trip() {
        let wc = WcConfig {
            endpoint: Some("unix:///run/scion/daemon.sock".into()),
            tracking_ref: Some("main".into()),
        };
        let text = toml::to_string(&wc).unwrap();
        assert_eq!(parse_wc(&text, &MapEnv::default()).unwrap(), wc);
        assert_eq!(
            parse_wc("", &MapEnv::default()).unwrap(),
            WcConfig::default()
        );
    }
}
