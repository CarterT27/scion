//! Sequential land model (SPEC 8.4).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::{MConflict, MDelta, MTree};

/// Apply each change's delta in order as a rebase onto the running result; the first failure
/// returns its index and conflicts.
pub fn land_sequential(
    base: &MTree,
    changes: &[Vec<MDelta>],
) -> Result<MTree, (usize, Vec<MConflict>)> {
    todo!("P07")
}
