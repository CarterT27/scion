//! Independent reference model used as the test oracle (SPEC 19.0; PLAN 2.20, 3.4).
//!
//! Written from SPEC.md and `docs/contracts/line-diff3-v1.md` alone. It depends on no scion
//! crate (checked by `scripts/check-layers.sh`) and works on structural values, so comparisons
//! are independent of production identifiers.

pub mod diff;
pub mod enc;

mod land;
mod merge;
mod model;
mod repo;
mod sparse;

pub use land::land_sequential;
pub use merge::merge_trees;
pub use model::{apply_delta, file_diff};
pub use repo::{MRebaseError, MRepo};
pub use sparse::materialize;

use std::collections::BTreeMap;

/// A repository path as raw bytes.
pub type MPath = Vec<u8>;

/// A file-level entry: mode 1..=3 plus content.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MFile {
    /// Mode 1 File, 2 Executable, 3 Symlink.
    pub mode: u8,
    /// Raw content (symlink target for mode 3).
    pub content: Vec<u8>,
}

/// A structural, file-level tree (no ids needed).
pub type MTree = BTreeMap<MPath, MFile>;

/// A structural delta entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MDelta {
    /// Put a file.
    Put(MPath, MFile),
    /// Delete a file.
    Delete(MPath),
}

/// A conflict found by the model; `kind` uses the SPEC 8.3 wire names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MConflict {
    /// Conflicting path.
    pub path: MPath,
    /// `add_add`, `modify_delete`, `symlink`, `binary`, `redacted`, `file_directory`,
    /// `content`.
    pub kind: &'static str,
}

/// Structural Meta fields for the model's own encoder (SPEC 4.3.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MMeta {
    /// Author bytes.
    pub author: Vec<u8>,
    /// `(seconds, tz_offset_minutes)`.
    pub author_time: (i64, i64),
    /// Optional committer.
    pub committer: Option<Vec<u8>>,
    /// Optional committer time.
    pub committer_time: Option<(i64, i64)>,
    /// Creator principal id bytes.
    pub creator: Vec<u8>,
    /// Message bytes.
    pub message: Vec<u8>,
    /// Extensions `(tag, value)`, ascending.
    pub ext: Vec<(u64, Vec<u8>)>,
}

/// Structural changeset fields for the model's own encoder (SPEC 4.3.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MChangeset {
    /// Parent ids (34 raw bytes each).
    pub parents: Vec<[u8; 34]>,
    /// Root tree id.
    pub root_tree: [u8; 34],
    /// Meta id.
    pub meta: [u8; 34],
    /// Change id.
    pub change_id: [u8; 16],
    /// Delta: `(path, Some((mode, blob id, size)))` for Put, `(path, None)` for Delete.
    #[allow(clippy::type_complexity)]
    pub delta: Vec<(MPath, Option<(u8, [u8; 34], u64)>)>,
}
