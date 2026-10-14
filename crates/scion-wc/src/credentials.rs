//! Client credentials file (SPEC 6.4, 12; PLAN 6.15; P11).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use scion_core::Result;
use std::path::Path;

/// `{token, github_token?}`; `Debug` never prints the secrets.
#[derive(Clone, PartialEq)]
pub struct Credentials {
    /// Scion token.
    pub token: String,
    /// GitHub OAuth token for silent re-exchange.
    pub github_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("token", &"***")
            .field("github_token", &self.github_token.as_ref().map(|_| "***"))
            .finish()
    }
}

/// Write with mode 0600 (parent 0700).
pub fn write(path: &Path, c: &Credentials) -> Result<()> {
    todo!("P11")
}

/// Read, refusing group- or world-readable files (PERMISSION_DENIED).
pub fn read(path: &Path) -> Result<Credentials> {
    todo!("P11")
}
