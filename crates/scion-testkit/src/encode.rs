//! Production encoding (scion-object) of structural trees (P09; exercised by P17).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use bytes::Bytes;
use scion_core::{Kind, ObjectId};

/// Encode `t` with scion-object: the root tree id plus every blob and tree object.
pub fn encode_tree(t: &scion_refmodel::MTree) -> (ObjectId, Vec<(ObjectId, Kind, Bytes)>) {
    todo!("P09")
}
