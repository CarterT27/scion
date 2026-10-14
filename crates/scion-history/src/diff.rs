//! FileDiff between stored trees with identical-subtree pruning (SPEC 4.3.4; P12).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::history::History;
use crate::types::FileChange;
use scion_core::{ObjectId, Result};
use scion_object::RepoPath;

impl History {
    /// File-level differences from tree `a` (`None` = empty tree) to tree `b`, ascending by
    /// path, optionally restricted to `scope` prefixes.
    pub async fn file_diff(
        &self,
        a: Option<&ObjectId>,
        b: &ObjectId,
        scope: Option<&[RepoPath]>,
    ) -> Result<Vec<FileChange>> {
        todo!("P12")
    }
}
