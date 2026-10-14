//! CockroachDB connection configuration.

use std::sync::Arc;

/// Connection settings (SPEC 6.1 `[metadata]`).
#[derive(Clone)]
pub struct CrdbConfig {
    /// `postgresql://...` URL.
    pub url: String,
    /// Pool size.
    pub max_connections: usize,
    /// Repository name (`repos.name`).
    pub repo_name: String,
    /// TLS client configuration; `None` connects without TLS.
    pub tls: Option<Arc<rustls::ClientConfig>>,
}

impl std::fmt::Debug for CrdbConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrdbConfig")
            .field("max_connections", &self.max_connections)
            .field("repo_name", &self.repo_name)
            .field("tls", &self.tls.is_some())
            .finish_non_exhaustive()
    }
}

/// `SCION_TEST_CRDB_URL`, the gate of the external CockroachDB tests (PLAN 3.3).
pub fn test_url() -> Option<String> {
    std::env::var("SCION_TEST_CRDB_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
}
