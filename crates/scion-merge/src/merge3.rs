//! line-diff3-v1 three-way content merge (SPEC 8.3).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use scion_core::ConflictHunk;

/// Result of a content merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Merge3 {
    /// Byte-exact merged content.
    Clean(Vec<u8>),
    /// Conflicting regions as base/ours/theirs line ranges.
    Conflict(Vec<ConflictHunk>),
}

/// Merge `ours` and `theirs` against `base` (no CRLF normalization; unterminated final lines
/// preserved).
pub fn merge3(base: &[u8], ours: &[u8], theirs: &[u8]) -> Merge3 {
    todo!("P02")
}
