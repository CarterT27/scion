//! Log(path) over history nodes (SPEC 7.4, 9.2; P24).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::engine::DerivedEngine;
use scion_core::{ObjectId, Result};
use scion_object::RepoPath;

/// One page of a path log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathLogPage {
    /// Changesets that changed the path, newest first.
    pub changesets: Vec<ObjectId>,
    /// Opaque token for the next page.
    pub next_page: Option<Vec<u8>>,
}

/// Changesets that changed `path`, walking history nodes from `from` (generation-descending).
pub async fn log_path(
    e: &DerivedEngine,
    from: &ObjectId,
    path: &RepoPath,
    first_parent_only: bool,
    limit: usize,
    page: Option<&[u8]>,
) -> Result<PathLogPage> {
    todo!("P24")
}
