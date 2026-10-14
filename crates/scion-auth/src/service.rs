//! Service principals (SPEC 12.3; P06).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use scion_core::Result;

/// Configured service principals: `(name, sha256 lowercase hex of the secret)`.
pub struct ServicePrincipals {
    entries: Vec<(String, [u8; 32])>,
}

impl ServicePrincipals {
    /// Validate entries (64 lowercase hex digests, unique names).
    pub fn new(entries: Vec<(String, String)>) -> Result<Self> {
        todo!("P06")
    }

    /// Always hashes `secret` and compares with [`crate::ct_eq`]; unknown names compare against
    /// a dummy digest so timing does not reveal which names exist.
    pub fn verify(&self, name: &str, secret: &[u8]) -> bool {
        todo!("P06")
    }
}
