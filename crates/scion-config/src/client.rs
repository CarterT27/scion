//! Client and working-copy configuration (SPEC 6.4, 11.1; P04).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::env::Env;
use crate::error::ConfigError;

/// `~/.config/scion/config.toml`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ClientConfig {
    /// `unix:///...` or `https://...`.
    pub endpoint: Option<String>,
}

/// `<root>/.scion/config.toml`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct WcConfig {
    /// Endpoint override for this working copy.
    pub endpoint: Option<String>,
    /// Tracked ref.
    pub tracking_ref: Option<String>,
}

/// Parse the user config.
pub fn parse_client(text: &str, env: &dyn Env) -> Result<ClientConfig, ConfigError> {
    todo!("P04")
}

/// Parse a working-copy config.
pub fn parse_wc(text: &str, env: &dyn Env) -> Result<WcConfig, ConfigError> {
    todo!("P04")
}

/// Endpoint precedence: flag > `SCION_ENDPOINT` > working-copy config > user config.
pub fn resolve_endpoint(
    flag: Option<&str>,
    env: &dyn Env,
    wc: Option<&WcConfig>,
    user: Option<&ClientConfig>,
) -> Option<String> {
    todo!("P04")
}
