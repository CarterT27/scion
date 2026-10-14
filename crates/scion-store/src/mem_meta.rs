//! Faithful hermetic in-memory [`MetaStore`] (PLAN 3.2).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::meta::MetaStore;
use crate::types::*;
use async_trait::async_trait;
use parking_lot::Mutex;
use scion_core::{ChangeId, ObjectId, Result};
use scion_object::RefName;
use std::time::{Duration, SystemTime};

/// Options of the in-memory metadata store.
#[derive(Clone, Debug)]
pub struct MemOptions {
    /// Repository name (`repos.name`).
    pub repo_name: String,
    /// How many commits stale reads lag behind (0: stale == strong).
    pub stale_lag_commits: usize,
    /// Every Nth transaction fails UNAVAILABLE/txn_retry with no effect.
    pub fail_txn_every: Option<u32>,
    /// Seed for any internal randomness.
    pub seed: u64,
}

impl Default for MemOptions {
    fn default() -> Self {
        MemOptions {
            repo_name: "test".to_owned(),
            stale_lag_commits: 0,
            fail_txn_every: None,
            seed: 0,
        }
    }
}

/// Mutable state guarded by the single mutex (filled in by P03 with `im` persistent maps).
#[derive(Debug, Default)]
struct MemState {
    txns: u64,
}

/// One `parking_lot::Mutex` over `im` persistent maps; a ring of the last K committed snapshots
/// serves [`Consistency::Stale`]. Each trait method is one atomic step (strictly
/// serializable).
#[derive(Debug)]
pub struct MemMetaStore {
    opts: MemOptions,
    state: Mutex<MemState>,
}

impl MemMetaStore {
    /// An empty store.
    pub fn new(opts: MemOptions) -> Self {
        MemMetaStore {
            opts,
            state: Mutex::new(MemState::default()),
        }
    }

    /// Change the stale-read lag.
    pub fn set_stale_lag(&self, commits: usize) {
        todo!("P03")
    }

    /// Advance the stale view to the latest committed snapshot.
    pub fn advance_stale_view(&self) {
        todo!("P03")
    }

    /// Row counts per table.
    pub fn table_counts(&self) -> TableCounts {
        todo!("P03")
    }

    /// Every objects row.
    pub fn all_objects(&self) -> Vec<ObjectRow> {
        todo!("P03")
    }

    /// Every changesets row.
    pub fn all_changesets(&self) -> Vec<ChangesetRow> {
        todo!("P03")
    }
}

#[async_trait]
impl MetaStore for MemMetaStore {
    fn repo_id(&self) -> RepoId {
        todo!("P03")
    }
    async fn ping(&self) -> Result<()> {
        todo!("P03")
    }
    async fn get_objects(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ObjectRow>>> {
        todo!("P03")
    }
    async fn insert_object(&self, row: ObjectRow) -> Result<InsertOutcome> {
        todo!("P03")
    }
    async fn record_upload(&self, id: &ObjectId, principal: &str, at: SystemTime) -> Result<()> {
        todo!("P03")
    }
    async fn has_upload(&self, id: &ObjectId, principal: &str) -> Result<bool> {
        todo!("P03")
    }
    async fn object_paths(&self, id: &ObjectId, limit: usize) -> Result<Vec<ObjectPathRow>> {
        todo!("P03")
    }
    async fn get_changesets(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ChangesetRow>>> {
        todo!("P03")
    }
    async fn changesets_by_prefix(&self, hex_prefix: &str, limit: usize) -> Result<Vec<ObjectId>> {
        todo!("P03")
    }
    async fn children(&self, parent: &ObjectId) -> Result<Vec<ObjectId>> {
        todo!("P03")
    }
    async fn has_root_changeset(&self) -> Result<bool> {
        todo!("P03")
    }
    async fn commit_changeset(&self, b: ChangesetBundle) -> Result<InsertOutcome> {
        todo!("P03")
    }
    async fn get_ref(&self, name: &RefName) -> Result<Option<RefRow>> {
        todo!("P03")
    }
    async fn list_refs(&self) -> Result<Vec<RefRow>> {
        todo!("P03")
    }
    async fn cas_ref(&self, cas: RefCas) -> Result<CasOutcome> {
        todo!("P03")
    }
    async fn ref_log(
        &self,
        name: &RefName,
        from_version: u64,
        limit: usize,
    ) -> Result<Vec<RefLogRow>> {
        todo!("P03")
    }
    async fn get_landed(&self, name: &RefName, change_id: &ChangeId) -> Result<Option<LandedRow>> {
        todo!("P03")
    }
    async fn get_derived(&self, k: &DerivedKey, c: Consistency) -> Result<Option<DerivedRow>> {
        todo!("P03")
    }
    async fn put_derived(&self, rows: Vec<DerivedRow>) -> Result<()> {
        todo!("P03")
    }
    async fn delete_derived_by_source(&self, types: &[String], source: &ObjectId) -> Result<u64> {
        todo!("P03")
    }
    async fn try_lease(
        &self,
        k: &DerivedKey,
        holder: &str,
        ttl: Duration,
        now: SystemTime,
    ) -> Result<bool> {
        todo!("P03")
    }
    async fn release_lease(&self, k: &DerivedKey, holder: &str) -> Result<()> {
        todo!("P03")
    }
    async fn redact(&self, t: RedactTxn) -> Result<u64> {
        todo!("P03")
    }
    async fn unredact(&self, ids: &[ObjectId], audit: AuditRow) -> Result<u64> {
        todo!("P03")
    }
    async fn mark_purged(&self, ids: &[ObjectId], audit: AuditRow) -> Result<()> {
        todo!("P03")
    }
    async fn redaction_epoch(&self) -> Result<u64> {
        todo!("P03")
    }
    async fn list_redactions(&self, since_epoch: u64) -> Result<(Vec<RedactionRow>, u64)> {
        todo!("P03")
    }
    async fn due_purges(&self, now: SystemTime, limit: usize) -> Result<Vec<ObjectId>> {
        todo!("P03")
    }
    async fn append_audit(&self, row: AuditRow) -> Result<()> {
        todo!("P03")
    }
    async fn audit_log(&self, limit: usize) -> Result<Vec<AuditRow>> {
        todo!("P03")
    }
}
