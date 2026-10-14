//! SPEC 9.2 history node, derived kind `0x10`.
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::canonical::Canonical;
use crate::enc::{Decoder, Encoder};
use crate::extensions::Extensions;
use crate::path::Name;
use scion_core::{Kind, ObjectId, Result};

/// SPEC 9.2 node kind: 1 File, 2 Dir.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// A file node.
    File = 1,
    /// A directory node.
    Dir = 2,
}

/// SPEC 9.2 `HistoryNode { kind, content, changeset, parents, children (Dir), extensions }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryNode {
    /// File or Dir.
    pub kind: NodeKind,
    /// Blob id (File) or Tree id (Dir).
    pub content: ObjectId,
    /// The changeset that introduced this content at this path.
    pub changeset: ObjectId,
    /// Nodes for this path in each parent where it exists (deduplicated, parent order).
    pub parents: Vec<ObjectId>,
    /// Dir only: `(name, node id)` ascending by name; empty for File.
    pub children: Vec<(Name, ObjectId)>,
    /// Extensions (final field).
    pub extensions: Extensions,
}

impl Canonical for HistoryNode {
    const KIND: Kind = Kind::Derived(0x10);

    fn encode_into(&self, e: &mut Encoder) {
        todo!("P01")
    }

    fn decode_from(d: &mut Decoder<'_>) -> Result<Self> {
        todo!("P01")
    }
}
