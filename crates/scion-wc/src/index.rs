//! The stat cache `.scion/index` (SPEC 11.5, 18.7; P11).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use scion_core::{Mode, ObjectId, Result};
use scion_object::RepoPath;
use std::collections::BTreeMap;
use std::path::Path;

/// `(size, mtime_ns, ctime_ns, inode, mode)` of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatTuple {
    /// Size in bytes.
    pub size: u64,
    /// Modification time (ns).
    pub mtime_ns: i128,
    /// Change time (ns).
    pub ctime_ns: i128,
    /// Inode number.
    pub inode: u64,
    /// Unix mode bits.
    pub mode: u32,
}

/// One index entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// Stat tuple at the last verification.
    pub stat: StatTuple,
    /// Blob id of the content.
    pub id: ObjectId,
    /// Entry mode.
    pub mode: Mode,
    /// True when the file is a redaction placeholder.
    pub redacted: bool,
}

/// Versioned binary stat cache, saved atomically.
pub struct StatCache {
    /// Entries by path.
    pub entries: BTreeMap<RepoPath, IndexEntry>,
    /// When the index was written (ns), for racy detection.
    pub written_at_ns: i128,
}

impl StatCache {
    /// Load from `<scion_dir>/index` (empty when absent).
    pub fn load(scion_dir: &Path) -> Result<Self> {
        todo!("P11")
    }

    /// Save atomically.
    pub fn save(&mut self, scion_dir: &Path) -> Result<()> {
        todo!("P11")
    }

    /// Racy when `mtime >= written_at` (SPEC 18.7).
    pub fn is_racy(&self, e: &IndexEntry) -> bool {
        todo!("P11")
    }
}
