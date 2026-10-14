//! SPEC 11.6 commit and amend (P28).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::wc::WorkingCopy;
use scion_core::{ObjectId, Result};
use scion_object::RepoPath;

impl WorkingCopy {
    /// Commit the working-copy changes (optionally only `paths`) on top of the base.
    pub async fn commit(
        &mut self,
        message: &[u8],
        author: &[u8],
        paths: Option<&[RepoPath]>,
    ) -> Result<ObjectId> {
        todo!("P28")
    }

    /// Amend the base changeset (same change id), optionally with a new message.
    pub async fn amend(&mut self, message: Option<&[u8]>) -> Result<ObjectId> {
        todo!("P28")
    }
}

/// Finish or roll back a pending Commit (called by `WorkingCopy::recover`).
pub(crate) async fn recover_commit(wc: &mut WorkingCopy) -> Result<Option<String>> {
    todo!("P28")
}
