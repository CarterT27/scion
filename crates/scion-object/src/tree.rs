//! SPEC 4.3.2 Tree.
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::canonical::Canonical;
use crate::enc::{Decoder, Encoder};
use crate::extensions::Extensions;
use crate::path::Name;
use scion_core::{Kind, Mode, ObjectId, Result};

/// One tree entry (SPEC 4.3.2): Blob id for modes 1-3, Tree id with size 0 for mode 4.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    /// Entry name.
    pub name: Name,
    /// Entry mode.
    pub mode: Mode,
    /// Entry object id.
    pub id: ObjectId,
    /// Blob length (modes 1-3) or 0 (mode 4).
    pub size: u64,
}

/// SPEC 4.3.2 Tree: `format_version = 1`, entries strictly ascending by name, extensions.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Tree {
    /// Entries, strictly ascending by bytewise name.
    pub entries: Vec<TreeEntry>,
    /// Extensions (final field).
    pub extensions: Extensions,
}

impl Canonical for Tree {
    const KIND: Kind = Kind::Tree;

    fn encode_into(&self, e: &mut Encoder) {
        todo!("P01")
    }

    /// Validates order, uniqueness, names, mode/kind agreement and size 0 for mode 4.
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self> {
        todo!("P01")
    }
}

impl Tree {
    /// The empty tree (valid only as a root; SPEC 4.3.2).
    pub fn empty() -> Self {
        Tree::default()
    }

    /// The entry named `name`, if present.
    pub fn get(&self, name: &[u8]) -> Option<&TreeEntry> {
        todo!("P01")
    }

    /// A non-root tree must have at least one entry (INVALID_ARGUMENT/empty_tree).
    pub fn validate_non_root(&self) -> Result<()> {
        todo!("P01")
    }
}
