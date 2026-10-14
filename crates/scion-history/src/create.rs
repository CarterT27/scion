//! SPEC 8.1 / 18.1 CreateChangeset with SPEC 5.3 write ordering (P12).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::history::History;
use crate::hooks::WriteAuthorizer;
use crate::types::{CreateRequest, Created, MetaSource};
use scion_core::{ChangeId, ObjectId, Principal, Result};
use scion_object::DeltaEntry;

impl History {
    /// Validate in the exact SPEC 8.1 order, build trees and Meta (creator = `who`), and
    /// persist per SPEC 5.3. Identical requests return the same id and never error.
    pub async fn create_changeset(
        &self,
        req: CreateRequest,
        who: &Principal,
        authz: &dyn WriteAuthorizer,
    ) -> Result<Created> {
        todo!("P12")
    }

    /// Internal creation used by rebase and merge: skips client-facing authorization and
    /// accepts an existing Meta.
    pub async fn create_internal(
        &self,
        parents: Vec<ObjectId>,
        delta: Vec<DeltaEntry>,
        meta: MetaSource,
        change_id: ChangeId,
        who: &Principal,
    ) -> Result<Created> {
        todo!("P12")
    }
}
