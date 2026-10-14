//! Myers forward greedy O(ND) diff with the normative tie-breaking (line-diff3-v1).
#![allow(unused_variables)] // P00 stub: remove when implemented.

/// One edit-script operation over line indices of `a` (old) and `b` (new).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// `len` equal lines starting at `a` and `b`.
    Equal {
        /// Start in `a`.
        a: usize,
        /// Start in `b`.
        b: usize,
        /// Run length.
        len: usize,
    },
    /// `len` lines of `a` deleted starting at `a`.
    Delete {
        /// Start in `a`.
        a: usize,
        /// Run length.
        len: usize,
    },
    /// `len` lines of `b` inserted starting at `b`.
    Insert {
        /// Start in `b`.
        b: usize,
        /// Run length.
        len: usize,
    },
}

/// The edit script from `a` to `b`, EXACTLY as `docs/contracts/line-diff3-v1.md`.
pub fn myers(a: &[&[u8]], b: &[&[u8]]) -> Vec<Edit> {
    todo!("P02")
}
