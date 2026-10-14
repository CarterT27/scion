//! SPEC 13.7 replacement of newly redacted clean files with placeholders (P18).

use crate::wc::WorkingCopy;
use scion_core::Result;
use scion_object::RepoPath;

impl WorkingCopy {
    /// Replace clean materialized copies of newly redacted blobs with placeholders; returns the
    /// dirty files that were left untouched (reported).
    pub async fn refresh_redactions(&mut self) -> Result<Vec<RepoPath>> {
        todo!("P18")
    }
}
