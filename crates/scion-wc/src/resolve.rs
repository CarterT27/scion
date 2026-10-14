//! SPEC 11.7 conflict resolution (P28).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::wc::WorkingCopy;
use scion_core::Result;
use scion_object::RepoPath;

impl WorkingCopy {
    /// Mark `p` resolved; refuses while marker lines remain unless `force`.
    pub fn resolve(&mut self, p: &RepoPath, force: bool) -> Result<()> {
        todo!("P28")
    }
}
