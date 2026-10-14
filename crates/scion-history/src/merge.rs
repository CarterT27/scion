//! SPEC 8.3 `Merge(a, b)` two-parent merge changesets (P19).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::history::History;
use crate::hooks::WriteAuthorizer;
use crate::types::MetaFields;
use scion_core::{ChangeId, ObjectId, Principal, Result};

impl History {
    /// Merge `b` into `a` over their unique merge base (empty tree when none;
    /// AMBIGUOUS_MERGE_BASE when several). Result parents are `[a, b]`.
    pub async fn merge(
        &self,
        a: &ObjectId,
        b: &ObjectId,
        meta: MetaFields,
        change_id: ChangeId,
        who: &Principal,
        authz: &dyn WriteAuthorizer,
    ) -> Result<ObjectId> {
        todo!("P19")
    }
}
