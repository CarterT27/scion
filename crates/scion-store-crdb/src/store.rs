//! The CockroachDB metadata store (P15).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::config::CrdbConfig;
use async_trait::async_trait;
use scion_core::{ChangeId, ObjectId, Result};
use scion_object::RefName;
use scion_store::*;
use std::time::{Duration, SystemTime};

/// SERIALIZABLE transactions, bounded 40001 retry, `AS OF SYSTEM TIME
/// follower_read_timestamp()` for [`Consistency::Stale`].
pub struct CrdbMetaStore {
    cfg: CrdbConfig,
}

impl CrdbMetaStore {
    /// Connect and ensure the `repos` row.
    pub async fn connect(cfg: CrdbConfig) -> Result<Self> {
        todo!("P15")
    }

    /// Apply [`crate::MIGRATIONS`] (idempotent).
    pub async fn migrate(&self) -> Result<()> {
        todo!("P15")
    }

    /// `CREATE DATABASE scion_test_<rand>`, connect and migrate (tests).
    pub async fn connect_fresh_for_tests(base_url: &str) -> Result<Self> {
        todo!("P15")
    }
}

#[async_trait]
impl MetaStore for CrdbMetaStore {
    fn repo_id(&self) -> RepoId {
        todo!("P15")
    }
    async fn ping(&self) -> Result<()> {
        todo!("P15")
    }
    async fn get_objects(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ObjectRow>>> {
        todo!("P15")
    }
    async fn insert_object(&self, row: ObjectRow) -> Result<InsertOutcome> {
        todo!("P15")
    }
    async fn record_upload(&self, id: &ObjectId, principal: &str, at: SystemTime) -> Result<()> {
        todo!("P15")
    }
    async fn has_upload(&self, id: &ObjectId, principal: &str) -> Result<bool> {
        todo!("P15")
    }
    async fn object_paths(&self, id: &ObjectId, limit: usize) -> Result<Vec<ObjectPathRow>> {
        todo!("P15")
    }
    async fn get_changesets(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ChangesetRow>>> {
        todo!("P15")
    }
    async fn changesets_by_prefix(&self, hex_prefix: &str, limit: usize) -> Result<Vec<ObjectId>> {
        todo!("P15")
    }
    async fn children(&self, parent: &ObjectId) -> Result<Vec<ObjectId>> {
        todo!("P15")
    }
    async fn has_root_changeset(&self) -> Result<bool> {
        todo!("P15")
    }
    async fn commit_changeset(&self, b: ChangesetBundle) -> Result<InsertOutcome> {
        todo!("P15")
    }
    async fn get_ref(&self, name: &RefName) -> Result<Option<RefRow>> {
        todo!("P15")
    }
    async fn list_refs(&self) -> Result<Vec<RefRow>> {
        todo!("P15")
    }
    async fn cas_ref(&self, cas: RefCas) -> Result<CasOutcome> {
        todo!("P15")
    }
    async fn ref_log(
        &self,
        name: &RefName,
        from_version: u64,
        limit: usize,
    ) -> Result<Vec<RefLogRow>> {
        todo!("P15")
    }
    async fn get_landed(&self, name: &RefName, change_id: &ChangeId) -> Result<Option<LandedRow>> {
        todo!("P15")
    }
    async fn get_derived(&self, k: &DerivedKey, c: Consistency) -> Result<Option<DerivedRow>> {
        todo!("P15")
    }
    async fn put_derived(&self, rows: Vec<DerivedRow>) -> Result<()> {
        todo!("P15")
    }
    async fn delete_derived_by_source(&self, types: &[String], source: &ObjectId) -> Result<u64> {
        todo!("P15")
    }
    async fn try_lease(
        &self,
        k: &DerivedKey,
        holder: &str,
        ttl: Duration,
        now: SystemTime,
    ) -> Result<bool> {
        todo!("P15")
    }
    async fn release_lease(&self, k: &DerivedKey, holder: &str) -> Result<()> {
        todo!("P15")
    }
    async fn redact(&self, t: RedactTxn) -> Result<u64> {
        todo!("P15")
    }
    async fn unredact(&self, ids: &[ObjectId], audit: AuditRow) -> Result<u64> {
        todo!("P15")
    }
    async fn mark_purged(&self, ids: &[ObjectId], audit: AuditRow) -> Result<()> {
        todo!("P15")
    }
    async fn redaction_epoch(&self) -> Result<u64> {
        todo!("P15")
    }
    async fn list_redactions(&self, since_epoch: u64) -> Result<(Vec<RedactionRow>, u64)> {
        todo!("P15")
    }
    async fn due_purges(&self, now: SystemTime, limit: usize) -> Result<Vec<ObjectId>> {
        todo!("P15")
    }
    async fn append_audit(&self, row: AuditRow) -> Result<()> {
        todo!("P15")
    }
    async fn audit_log(&self, limit: usize) -> Result<Vec<AuditRow>> {
        todo!("P15")
    }
}
