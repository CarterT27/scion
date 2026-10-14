//! Reusable client scenarios (feature `testing`; P18). Write scenarios (P28) are re-exported
//! from `testing_write`.
#![allow(unused_variables)] // P00 stub: remove when implemented.

#[allow(unused_imports)] // P00: testing_write is empty until P28.
pub use crate::testing_write::*;

use scion_core::{ObjectId, Result};
use scion_object::RepoPath;
use scion_proto::ScionClient;
use std::collections::BTreeSet;

/// A client plus a temporary root for working copies.
pub struct Scenario {
    /// Client for the stack under test.
    pub client: ScionClient,
    /// Temporary directory for working copies.
    pub root: tempfile::TempDir,
}

/// Object ids fetched by a sparse checkout of `dir` at `rev` (SPEC 19.10).
pub async fn sparse_object_set(
    s: &Scenario,
    rev: &str,
    dir: &RepoPath,
) -> Result<BTreeSet<ObjectId>> {
    todo!("P18")
}
