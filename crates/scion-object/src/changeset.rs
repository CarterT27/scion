//! SPEC 4.3.4 Changeset.
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::canonical::Canonical;
use crate::enc::{Decoder, Encoder};
use crate::extensions::Extensions;
use crate::path::RepoPath;
use scion_core::{ChangeId, Kind, Limits, Mode, ObjectId, Result};

/// SPEC 4.3.4 delta operation: `1 Put (mode 1..=3, Blob id, size)` or `2 Delete`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeltaOp {
    /// Put a file-level entry.
    Put {
        /// Mode 1..=3.
        mode: Mode,
        /// Blob id.
        id: ObjectId,
        /// Blob length.
        size: u64,
    },
    /// Delete a file.
    Delete,
}

/// SPEC 4.3.4 DeltaEntry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeltaEntry {
    /// File path.
    pub path: RepoPath,
    /// Operation.
    pub op: DeltaOp,
}

/// SPEC 4.3.4 Changeset. `delta` equals `FileDiff(T1, root_tree)` relative to the first
/// parent only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changeset {
    /// Parent changesets: ordered, unique, `0..=limits.max_parents`.
    pub parents: Vec<ObjectId>,
    /// Root tree id.
    pub root_tree: ObjectId,
    /// Meta id.
    pub meta: ObjectId,
    /// Change id (16 bytes).
    pub change_id: ChangeId,
    /// Delta, strictly ascending by path.
    pub delta: Vec<DeltaEntry>,
    /// Extensions (final field).
    pub extensions: Extensions,
}

impl Canonical for Changeset {
    const KIND: Kind = Kind::Changeset;

    fn encode_into(&self, e: &mut Encoder) {
        todo!("P01")
    }

    fn decode_from(d: &mut Decoder<'_>) -> Result<Self> {
        todo!("P01")
    }
}

impl Changeset {
    /// Shape checks of SPEC 8.1 steps 1-2 that need no storage: parent uniqueness and count
    /// (TOO_LARGE `max_parents`), delta sorted/unique (delta_unsorted / delta_duplicate),
    /// `max_delta_entries` (TOO_LARGE) and path rules (invalid_path).
    pub fn validate_shape(&self, limits: &Limits) -> Result<()> {
        todo!("P01")
    }
}
