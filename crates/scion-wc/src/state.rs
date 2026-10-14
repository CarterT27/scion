//! `.scion/state`: canonical TOML, saved atomically (SPEC 4.3.10, 11.1; P11).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use scion_core::{ChangeId, ObjectId, Result};
use scion_object::{RefName, RepoPath};
use std::path::Path;

/// A journaled multi-step operation (SPEC 11.3, 11.6, 11.7).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PendingOperation {
    /// Initial checkout of `target`.
    Checkout {
        /// Target changeset.
        target: ObjectId,
    },
    /// Goto from `from` to `to`.
    Goto {
        /// Previous base.
        from: ObjectId,
        /// New base.
        to: ObjectId,
    },
    /// Sparse profile change.
    SparseChange {
        /// Old profile.
        #[serde(with = "crate::serde_util::paths")]
        old: Vec<RepoPath>,
        /// New profile.
        #[serde(with = "crate::serde_util::paths")]
        new: Vec<RepoPath>,
    },
    /// Commit or amend on top of `parent`.
    Commit {
        /// Parent changeset.
        parent: ObjectId,
        /// Change id of the new changeset.
        change_id: ChangeId,
        /// True for amend.
        amend: bool,
    },
    /// Stack rebase.
    Rebase {
        /// Original head.
        original: ObjectId,
        /// Destination.
        onto: ObjectId,
        /// Members still to rebase.
        remaining_stack: Vec<ObjectId>,
        /// Rebased results so far.
        done: Vec<ObjectId>,
        /// Conflicting paths (raw bytes).
        conflicts: Vec<Vec<u8>>,
        /// Resolved paths (raw bytes).
        resolved: Vec<Vec<u8>>,
        /// Member being rebased.
        current: Option<ObjectId>,
    },
}

/// Working-copy state (SPEC 4.3.10).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WcState {
    /// Endpoint override.
    pub endpoint: Option<String>,
    /// Base changeset.
    pub base: ObjectId,
    /// Tracked ref.
    #[serde(default, with = "crate::serde_util::opt_ref")]
    pub tracking_ref: Option<RefName>,
    /// Sparse profile prefixes.
    #[serde(with = "crate::serde_util::paths")]
    pub sparse: Vec<RepoPath>,
    /// Prefixes the caller cannot read.
    #[serde(with = "crate::serde_util::paths")]
    pub inaccessible: Vec<RepoPath>,
    /// Journaled operation, if any.
    pub pending: Option<PendingOperation>,
}

/// Load `<scion_dir>/state`.
pub fn load(scion_dir: &Path) -> Result<WcState> {
    todo!("P11")
}

/// Save `<scion_dir>/state` as canonical TOML, atomically.
pub fn save(scion_dir: &Path, s: &WcState) -> Result<()> {
    todo!("P11")
}
