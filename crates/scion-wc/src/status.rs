//! SPEC 11.5 / 18.7 status over a base-tree lookup (P11).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::fs_safe::SafeRoot;
use crate::ignore::IgnoreMatcher;
use crate::index::StatCache;
use scion_core::{EntryInfo, Result};
use scion_object::RepoPath;
use std::collections::BTreeMap;

/// Base tree access for status (implemented over the local tree cache).
pub trait TreeLookup: Send + Sync {
    /// Every file-level entry of the base tree under the scope prefixes.
    fn files_in_scope(&self, scope: &[RepoPath]) -> Result<BTreeMap<RepoPath, EntryInfo>>;
}

/// Status result.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct StatusReport {
    /// Content changed.
    #[serde(with = "crate::serde_util::paths")]
    pub modified: Vec<RepoPath>,
    /// Not in the base tree.
    #[serde(with = "crate::serde_util::paths")]
    pub added: Vec<RepoPath>,
    /// In the base tree, missing on disk.
    #[serde(with = "crate::serde_util::paths")]
    pub deleted: Vec<RepoPath>,
    /// Same content, different mode.
    #[serde(with = "crate::serde_util::paths")]
    pub mode_changed: Vec<RepoPath>,
    /// Edited redaction placeholders (also listed in `modified`).
    #[serde(with = "crate::serde_util::paths")]
    pub redacted_dirty: Vec<RepoPath>,
    /// Number of files hashed.
    pub hashed: usize,
}

/// Walk in-scope files (skipping `.scion`, ignored files and inaccessible prefixes), hash only
/// changed or racy entries, and compare with `base`.
pub fn status(
    root: &SafeRoot,
    idx: &mut StatCache,
    ign: &IgnoreMatcher,
    scope: &[RepoPath],
    inaccessible: &[RepoPath],
    base: &dyn TreeLookup,
) -> Result<StatusReport> {
    todo!("P11")
}
