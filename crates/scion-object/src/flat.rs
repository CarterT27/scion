//! Flat file-level trees and the pure SPEC 4.3.4 FileDiff.
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::changeset::DeltaEntry;
use crate::path::RepoPath;
use scion_core::EntryInfo;
use std::collections::BTreeMap;

/// File-level entries only (modes 1-3), keyed by path.
pub type FlatTree = BTreeMap<RepoPath, EntryInfo>;

/// SPEC 4.3.4 FileDiff: every path whose `(mode, id)` differs, Put when present in `b`,
/// Delete when absent; ascending by path.
pub fn file_diff(a: &FlatTree, b: &FlatTree) -> Vec<DeltaEntry> {
    todo!("P01")
}
