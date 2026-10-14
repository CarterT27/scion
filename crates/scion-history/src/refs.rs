//! Refs, UpdateRef and the ref log (SPEC 4.3.6, 4.3.7, 8.5; P20).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::history::History;
use crate::hooks::LandAuthorizer;
use crate::types::UpdateRefRequest;
use scion_core::{ObjectId, Principal, Result};
use scion_object::RefName;
use scion_store::{RefLogRow, RefRow};

impl History {
    /// Strong read of a ref; `at_version` is served from the ref log (NOT_FOUND when absent).
    pub async fn resolve_ref(&self, name: &RefName, at_version: Option<u64>) -> Result<RefRow> {
        todo!("P20")
    }

    /// SPEC 8.5 version-checked update with fast-forward enforcement and ref log.
    pub async fn update_ref(
        &self,
        req: UpdateRefRequest,
        who: &Principal,
        authz: &dyn LandAuthorizer,
    ) -> Result<RefRow> {
        todo!("P20")
    }

    /// Create a ref (INVALID_ARGUMENT/ref_exists when it exists). Its version is 1, or continues
    /// after the last logged version of a previously deleted ref (storage-semantics 4.3). The
    /// caller checks admin.refs.
    pub async fn create_ref(
        &self,
        name: &RefName,
        target: &ObjectId,
        who: &Principal,
        reason: &str,
    ) -> Result<RefRow> {
        todo!("P20")
    }

    /// Delete a ref at `expected_version`; the caller checks admin.refs.
    pub async fn delete_ref(
        &self,
        name: &RefName,
        expected_version: u64,
        who: &Principal,
        reason: &str,
    ) -> Result<()> {
        todo!("P20")
    }

    /// Ref log entries from `from_version`, ascending.
    pub async fn ref_log(
        &self,
        name: &RefName,
        from_version: u64,
        limit: usize,
    ) -> Result<Vec<RefLogRow>> {
        todo!("P20")
    }
}
