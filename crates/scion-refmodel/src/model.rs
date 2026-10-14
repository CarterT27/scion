//! Delta application and FileDiff over structural trees (SPEC 4.3.4, 18.2).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::{MDelta, MTree};

/// Apply a delta; invalid deltas return the SPEC 8.1 subcode (`delete_absent`,
/// `delete_not_file`, `noop_put`, `file_directory`).
pub fn apply_delta(t: &MTree, d: &[MDelta]) -> Result<MTree, &'static str> {
    todo!("P07")
}

/// SPEC 4.3.4 FileDiff, ascending by path.
pub fn file_diff(a: &MTree, b: &MTree) -> Vec<MDelta> {
    todo!("P07")
}
