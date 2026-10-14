//! SPEC 8.3 tree-level three-way merge over structural trees.
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::{MConflict, MPath, MTree};
use std::collections::BTreeSet;

/// Merge `o` and `t` against `b`. `paths` restricts the examined paths (rebase: the delta
/// paths); `redacted` lists contents treated as redacted.
pub fn merge_trees(
    b: &MTree,
    o: &MTree,
    t: &MTree,
    paths: Option<&BTreeSet<MPath>>,
    max_text: u64,
    redacted: &BTreeSet<Vec<u8>>,
) -> Result<MTree, Vec<MConflict>> {
    todo!("P07")
}
