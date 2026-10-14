//! Vendored git histories and git fixtures (SPEC 19.0, 19.14; P09).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use scion_core::Result;
use std::path::{Path, PathBuf};

/// Every vendored bundle under `corpus/git/`.
pub fn bundles() -> Vec<PathBuf> {
    todo!("P09")
}

/// Unpack `bundle` into a bare repository under `dst` (via gix; no git CLI); returns its path.
pub fn unpack(bundle: &Path, dst: &Path) -> Result<PathBuf> {
    todo!("P09")
}

/// True when the `git` CLI is available (fixture builders and differential tests).
pub fn git_available() -> bool {
    todo!("P09")
}

/// A temporary git repository built by the `git` CLI from a script.
pub struct GitFixture {
    /// The repository directory.
    pub dir: tempfile::TempDir,
}

impl GitFixture {
    /// Run `script` with the git CLI (callers skip when [`git_available`] is false).
    pub fn build(script: &[GitOp]) -> Result<Self> {
        todo!("P09")
    }
}

/// One fixture step.
#[derive(Clone, Debug)]
pub enum GitOp {
    /// Commit on `branch` writing `files` as `(path, git mode, content)`, removing `deletes`,
    /// and adding `gitlinks` as `(path, 20-byte sha)`.
    Commit {
        /// Branch.
        branch: String,
        /// Files to write.
        files: Vec<(String, u32, Vec<u8>)>,
        /// Paths to delete.
        deletes: Vec<String>,
        /// Gitlinks to add.
        gitlinks: Vec<(String, [u8; 20])>,
        /// Message.
        message: String,
    },
    /// Merge `from` branches into `into` (more than one = octopus).
    Merge {
        /// Target branch.
        into: String,
        /// Merged branches.
        from: Vec<String>,
        /// Message.
        message: String,
    },
    /// Start an orphan branch (a second root).
    OrphanRoot {
        /// Branch.
        branch: String,
    },
    /// An empty commit.
    EmptyCommit {
        /// Branch.
        branch: String,
        /// Message.
        message: String,
    },
}
