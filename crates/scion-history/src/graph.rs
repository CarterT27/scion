//! Commit-graph queries (SPEC 9.2, 18.5; P13).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::history::History;
use scion_core::{ObjectId, Result};

impl History {
    /// SPEC 18.5: is `a` an ancestor of (or equal to) `b`? Generation-pruned search.
    pub async fn is_ancestor(&self, a: &ObjectId, b: &ObjectId) -> Result<bool> {
        todo!("P13")
    }

    /// SPEC 18.5: first-parent ancestry via skip pointers, O(log n).
    pub async fn is_first_parent_ancestor(&self, a: &ObjectId, b: &ObjectId) -> Result<bool> {
        todo!("P13")
    }

    /// All best common ancestors, sorted by id (SPEC 8.3).
    pub async fn merge_bases(&self, a: &ObjectId, b: &ObjectId) -> Result<Vec<ObjectId>> {
        todo!("P13")
    }

    /// `c~n`: the n-th first-parent ancestor (SPEC 11.8).
    pub async fn nth_first_parent(&self, c: &ObjectId, n: u32) -> Result<Option<ObjectId>> {
        todo!("P13")
    }

    /// `c^k`: the k-th parent, k >= 1 (SPEC 11.8).
    pub async fn nth_parent(&self, c: &ObjectId, k: u32) -> Result<Option<ObjectId>> {
        todo!("P13")
    }

    /// One page of history from `from`, newest first by generation then id; `after` is the
    /// opaque page token returned by the previous page.
    pub async fn log_page(
        &self,
        from: &ObjectId,
        first_parent_only: bool,
        limit: usize,
        after: Option<&[u8]>,
    ) -> Result<(Vec<ObjectId>, Option<Vec<u8>>)> {
        todo!("P13")
    }
}
