//! Endpoint parsing and TLS options (SPEC 6.4, 7.1; P08).

use scion_core::Result;
use std::path::PathBuf;

/// Where a client connects: a daemon Unix socket or an HTTPS server (SPEC 6.4).
#[derive(Clone, Debug)]
pub enum Endpoint {
    /// `unix:///abs/path`.
    Unix(PathBuf),
    /// `https://host:port`.
    Https(String),
}

impl Endpoint {
    /// Parse `unix:///abs/path` or `https://host:port` (INVALID_ARGUMENT otherwise).
    pub fn parse(s: &str) -> Result<Self> {
        let _ = s;
        todo!("P08")
    }
}

/// TLS client options; without a CA the native roots are used (SPEC 17).
#[derive(Clone, Debug, Default)]
pub struct TlsOptions {
    /// PEM-encoded CA certificate(s) to trust.
    pub ca_pem: Option<Vec<u8>>,
    /// Server name override for certificate verification.
    pub domain: Option<String>,
}
