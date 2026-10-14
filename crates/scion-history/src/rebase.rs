//! SPEC 8.2 / 18.3 deterministic rebase (P19).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::history::History;
use crate::hooks::Rebaser;
use async_trait::async_trait;
use scion_core::{ObjectId, Principal, Result};

impl History {
    /// Rebase `c` onto `onto`: MERGE_CONFLICT (with ConflictReport) or EMPTY_CHANGE on
    /// failure; INVALID_ARGUMENT/multi_parent for merges. A pure function of its inputs.
    pub async fn rebase(&self, c: &ObjectId, onto: &ObjectId, who: &Principal) -> Result<ObjectId> {
        todo!("P19")
    }
}

#[async_trait]
impl Rebaser for History {
    async fn rebase(&self, c: &ObjectId, onto: &ObjectId, who: &Principal) -> Result<ObjectId> {
        todo!("P19")
    }
}
