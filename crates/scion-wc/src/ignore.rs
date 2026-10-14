//! gitignore semantics (SPEC 11.4; P11).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use scion_core::Result;
use scion_object::RepoPath;
use std::path::Path;

/// Nested `.gitignore` matcher with git semantics.
pub struct IgnoreMatcher {
    _private: (),
}

impl IgnoreMatcher {
    /// Load every `.gitignore` under `root`.
    pub fn load(root: &Path) -> Result<Self> {
        todo!("P11")
    }

    /// True when `p` is ignored.
    pub fn is_ignored(&self, p: &RepoPath, is_dir: bool) -> bool {
        todo!("P11")
    }
}
