//! The model commit graph (SPEC 8.2, 9.2, 18.5).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::{MConflict, MTree};

/// Why a model rebase failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MRebaseError {
    /// Conflicts (SPEC 8.3).
    Conflict(Vec<MConflict>),
    /// EMPTY_CHANGE (SPEC 8.2).
    Empty,
}

/// A naive in-memory repository; commits are indexed `0..n` in insertion order.
#[derive(Clone, Debug, Default)]
pub struct MRepo {
    commits: Vec<(Vec<usize>, MTree)>,
}

impl MRepo {
    /// An empty repository.
    pub fn new() -> Self {
        MRepo {
            commits: Vec::new(),
        }
    }

    /// Add a commit with the given parents and tree; returns its index.
    pub fn commit(&mut self, parents: &[usize], tree: MTree) -> usize {
        todo!("P07")
    }

    /// The tree of commit `c`.
    pub fn tree(&self, c: usize) -> &MTree {
        todo!("P07")
    }

    /// SPEC 9.2 generation (root = 1).
    pub fn generation(&self, c: usize) -> u64 {
        todo!("P07")
    }

    /// SPEC 9.2 skip pointers: first-parent ancestors at distance 2^k.
    pub fn skip(&self, c: usize) -> Vec<usize> {
        todo!("P07")
    }

    /// Is `a` an ancestor of (or equal to) `b` (full BFS).
    pub fn is_ancestor(&self, a: usize, b: usize) -> bool {
        todo!("P07")
    }

    /// All best common ancestors.
    pub fn merge_bases(&self, a: usize, b: usize) -> Vec<usize> {
        todo!("P07")
    }

    /// SPEC 8.2 rebase of `c` onto `onto`.
    pub fn rebase(&mut self, c: usize, onto: usize, max_text: u64) -> Result<usize, MRebaseError> {
        todo!("P07")
    }

    /// Log of path `p` from `from` with TREESAME semantics equivalent to SPEC 9.2 node reuse.
    pub fn log_path(&self, from: usize, p: &[u8], first_parent_only: bool) -> Vec<usize> {
        todo!("P07")
    }

    /// SPEC 9.2 blame: `(start_line, line_count, origin_commit, origin_start_line)`.
    pub fn blame(&self, c: usize, p: &[u8]) -> Vec<(u32, u32, usize, u32)> {
        todo!("P07")
    }

    /// Distinct contents ever held at `p` in history reachable from `c`.
    pub fn all_versions(&self, c: usize, p: &[u8]) -> Vec<Vec<u8>> {
        todo!("P07")
    }
}
