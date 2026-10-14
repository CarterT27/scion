//! SPEC 11.2 sparse profile changes (P18).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::wc::WorkingCopy;
use scion_core::Result;
use scion_object::RepoPath;

impl WorkingCopy {
    /// Add prefixes to the sparse profile and materialize exactly the affected files.
    pub async fn sparse_add(&mut self, p: &[RepoPath]) -> Result<()> {
        todo!("P18")
    }

    /// Remove prefixes; fails on dirty files unless `force`.
    pub async fn sparse_remove(&mut self, p: &[RepoPath], force: bool) -> Result<()> {
        todo!("P18")
    }
}
