//! The model's own canonical encoder (SPEC 4.1-4.3), independent of scion-object.
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::{MChangeset, MMeta, MTree};

/// Blob id: `0x01 0x01 || BLAKE3(content)`.
pub fn blob_id(c: &[u8]) -> [u8; 34] {
    todo!("P07")
}

/// Root tree id of a flat tree (trees built recursively).
pub fn tree_id(t: &MTree) -> [u8; 34] {
    todo!("P07")
}

/// Canonical bytes of the tree for directory `dir` (empty = root) of `t`.
pub fn tree_bytes_for_dir(t: &MTree, dir: &[u8]) -> Vec<u8> {
    todo!("P07")
}

/// Canonical Meta bytes.
pub fn meta_bytes(m: &MMeta) -> Vec<u8> {
    todo!("P07")
}

/// Canonical Changeset bytes.
pub fn changeset_bytes(c: &MChangeset) -> Vec<u8> {
    todo!("P07")
}

/// SPEC 4.2 text form of a raw id.
pub fn id_text(id: &[u8; 34]) -> String {
    todo!("P07")
}
