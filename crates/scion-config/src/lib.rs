//! Configuration (SPEC 6; PLAN 2.11, 6.11): server, daemon, client and working-copy TOML
//! files with `$VAR` resolution, unknown-key rejection naming the full key, and SPEC 6.5
//! defaults.

mod client;
mod daemon;
mod env;
mod error;
mod server;

pub use client::{ClientConfig, WcConfig, parse_client, parse_wc, resolve_endpoint};
pub use daemon::{DaemonConfig, load_daemon, parse_daemon};
pub use env::{Env, MapEnv, ProcessEnv};
pub use error::ConfigError;
pub use server::{
    AuthSection, DeriveSection, LandSection, MetadataSection, ObjectsSection, ObservabilitySection,
    PolicySection, RateLimitSection, RedactionSection, ServerConfig, ServerSection,
    ServicePrincipalConfig, load_server, parse_server,
};
