//! Tree reading through the ObjectRepo caches (P12).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::history::History;
use crate::types::ChangesetRecord;
use scion_core::{ObjectId, Result};
use scion_object::{RepoPath, Tree, TreeEntry};
use std::sync::Arc;

impl History {
    /// A decoded tree (SPEC 4.3.2).
    pub async fn tree(&self, id: &ObjectId) -> Result<Arc<Tree>> {
        todo!("P12")
    }

    /// The entry at `path` under `root` (`None` when absent; the root path has no entry).
    pub async fn lookup(&self, root: &ObjectId, path: &RepoPath) -> Result<Option<TreeEntry>> {
        todo!("P12")
    }

    /// A decoded changeset with its commit-graph columns.
    pub async fn changeset(&self, id: &ObjectId) -> Result<ChangesetRecord> {
        todo!("P12")
    }
}
