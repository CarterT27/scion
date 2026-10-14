//! SPEC 18.2 apply_delta (P12).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::history::History;
use crate::types::Applied;
use scion_core::{ObjectId, Result};
use scion_object::DeltaEntry;

impl History {
    /// Build the result tree of applying `delta` to tree `t1`, walking only the ancestors of
    /// delta paths (O(|delta| x depth)). Subcodes: delete_absent, delete_not_file,
    /// file_directory, noop_put.
    pub async fn apply_delta(&self, t1: &ObjectId, delta: &[DeltaEntry]) -> Result<Applied> {
        todo!("P12")
    }
}
