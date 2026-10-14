//! All versions of a path (SPEC 13 redaction tooling; P24).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::engine::DerivedEngine;
use scion_core::{ObjectId, Result};
use scion_object::RepoPath;

/// Distinct blob ids ever held at `path` in history reachable from `at`, first-seen order.
pub async fn all_versions(
    e: &DerivedEngine,
    at: &ObjectId,
    path: &RepoPath,
) -> Result<Vec<ObjectId>> {
    todo!("P24")
}
