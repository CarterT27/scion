//! Hunk extraction from the Myers edit script (line-diff3-v1).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use std::ops::Range;

/// A changed region: `base` lines replaced by `new` lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// Replaced range in the old text.
    pub base: Range<usize>,
    /// Replacement range in the new text.
    pub new: Range<usize>,
}

/// Group the non-equal runs of `myers(a, b)` into hunks.
pub fn diff_lines(a: &[&[u8]], b: &[&[u8]]) -> Vec<Hunk> {
    todo!("P02")
}
