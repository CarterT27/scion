//! Safe filesystem access under the working-copy root (SPEC 11.3, 17; P11).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::index::StatTuple;
use scion_core::{Mode, Result};
use scion_object::RepoPath;
use std::path::{Path, PathBuf};

/// The working-copy root opened with `O_NOFOLLOW` traversal: nothing is ever written outside
/// it and symlinked parents are refused.
pub struct SafeRoot {
    root: PathBuf,
}

impl SafeRoot {
    /// Open the root directory.
    pub fn open(root: &Path) -> Result<Self> {
        todo!("P11")
    }

    /// Write via temp file plus rename in the target directory (exec bit for Executable).
    pub fn write_file_atomic(&self, p: &RepoPath, data: &[u8], mode: Mode) -> Result<()> {
        todo!("P11")
    }

    /// Create a symlink.
    pub fn symlink(&self, p: &RepoPath, target: &[u8]) -> Result<()> {
        todo!("P11")
    }

    /// Remove a file or symlink (pruning empty parents).
    pub fn remove(&self, p: &RepoPath) -> Result<()> {
        todo!("P11")
    }

    /// Read a file.
    pub fn read(&self, p: &RepoPath) -> Result<Vec<u8>> {
        todo!("P11")
    }

    /// Stat without following symlinks; `None` when absent.
    pub fn stat(&self, p: &RepoPath) -> Result<Option<StatTuple>> {
        todo!("P11")
    }

    /// Read a symlink target.
    pub fn read_link(&self, p: &RepoPath) -> Result<Vec<u8>> {
        todo!("P11")
    }

    /// Whether the filesystem is case-insensitive (probed once).
    pub fn is_case_insensitive(&self) -> bool {
        todo!("P11")
    }
}

/// SPEC 4.4 plus never `.scion` in any case.
pub fn validate_entry_path(p: &[u8]) -> Result<RepoPath> {
    todo!("P11")
}

/// ASCII case-fold collisions (deterministic on any filesystem; PLAN 6.17).
pub fn case_collisions(paths: &[RepoPath]) -> Vec<(RepoPath, RepoPath)> {
    todo!("P11")
}
