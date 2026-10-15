//! The metadata store trait (SPEC 5.2-5.4).
//!
//! Every method is exactly ONE serializable transaction (`docs/contracts/storage-semantics.md`).
//! Infrastructure failures are UNAVAILABLE (subcode `store_unavailable`, `txn_retry` or
//! `injected*`) and never leave partial effects.

use crate::types::*;
use async_trait::async_trait;
use scion_core::{ChangeId, ObjectId, Result};
use scion_object::RefName;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// SPEC 5.2 metadata store. Object-safe (`Arc<dyn MetaStore>`).
#[async_trait]
pub trait MetaStore: Send + Sync + 'static {
    /// The repository id all rows are keyed by (SPEC 5.2).
    fn repo_id(&self) -> RepoId;
    /// Liveness probe used by readiness checks (SPEC 15.3).
    async fn ping(&self) -> Result<()>;
    /// Object rows for `ids`, in order (`None` when absent). Stale reads may miss recent rows
    /// (SPEC 5.4).
    async fn get_objects(&self, ids: &[ObjectId], c: Consistency)
    -> Result<Vec<Option<ObjectRow>>>;
    /// Insert an objects row, ON CONFLICT DO NOTHING (SPEC 5.3 step 1).
    async fn insert_object(&self, row: ObjectRow) -> Result<InsertOutcome>;
    /// [`MetaStore::insert_object`] for every row, in input order (a repeated id is
    /// `AlreadyExists` after its first occurrence). Stores override this with one transaction.
    async fn insert_objects(&self, rows: Vec<ObjectRow>) -> Result<Vec<InsertOutcome>> {
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(self.insert_object(row).await?);
        }
        Ok(out)
    }
    /// Record that `principal` uploaded `id` (`object_uploads`, SPEC 12.5).
    async fn record_upload(&self, id: &ObjectId, principal: &str, at: SystemTime) -> Result<()>;
    /// Record all uploads for one principal atomically, deduplicating existing records.
    async fn record_uploads(&self, ids: &[ObjectId], principal: &str, at: SystemTime)
    -> Result<()>;
    /// True when `principal` uploaded `id`.
    async fn has_upload(&self, id: &ObjectId, principal: &str) -> Result<bool>;
    /// Strong ownership checks in input order (including duplicates), in one transaction.
    async fn has_uploads(&self, ids: &[ObjectId], principal: &str) -> Result<Vec<bool>>;
    /// `object_paths` rows for this id only, ascending by path, at most `limit` (SPEC 12.5).
    async fn object_paths(&self, id: &ObjectId, limit: usize) -> Result<Vec<ObjectPathRow>>;
    /// [`MetaStore::object_paths`] for every id, in input order (including duplicates), at
    /// most `limit` rows per id. Stores override this with one metadata call.
    async fn object_paths_many(
        &self,
        ids: &[ObjectId],
        limit: usize,
    ) -> Result<Vec<Vec<ObjectPathRow>>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.object_paths(id, limit).await?);
        }
        Ok(out)
    }
    /// Changeset rows for `ids`, in order.
    async fn get_changesets(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ChangesetRow>>>;
    /// Changeset ids whose digest hex starts with `hex_prefix`, ascending, at most `limit`.
    async fn changesets_by_prefix(&self, hex_prefix: &str, limit: usize) -> Result<Vec<ObjectId>>;
    /// Children of `parent` (`changeset_children`), ascending.
    async fn children(&self, parent: &ObjectId) -> Result<Vec<ObjectId>>;
    /// True once any parentless changeset exists (SPEC 8.1 step 1).
    async fn has_root_changeset(&self) -> Result<bool>;
    /// One transaction inserting the changeset row, children, object_paths and eager derived
    /// rows, each ON CONFLICT DO NOTHING independently (SPEC 5.3 step 2).
    async fn commit_changeset(&self, b: ChangesetBundle) -> Result<InsertOutcome>;
    /// Strong read of a ref (SPEC 5.4).
    async fn get_ref(&self, name: &RefName) -> Result<Option<RefRow>>;
    /// All refs, ascending by name (strong).
    async fn list_refs(&self) -> Result<Vec<RefRow>>;
    /// Version-checked ref update plus ref_log, landed and audit rows in one transaction
    /// (SPEC 5.3 step 3, 8.4, 8.5).
    async fn cas_ref(&self, cas: RefCas) -> Result<CasOutcome>;
    /// Ref log entries with `version >= from_version`, ascending, at most `limit`.
    async fn ref_log(
        &self,
        name: &RefName,
        from_version: u64,
        limit: usize,
    ) -> Result<Vec<RefLogRow>>;
    /// The `landed` row for `(ref, change_id)` (SPEC 8.4).
    async fn get_landed(&self, name: &RefName, change_id: &ChangeId) -> Result<Option<LandedRow>>;
    /// A derived row (SPEC 9.1).
    async fn get_derived(&self, k: &DerivedKey, c: Consistency) -> Result<Option<DerivedRow>>;
    /// Insert derived rows, ON CONFLICT DO NOTHING (SPEC 9.1).
    async fn put_derived(&self, rows: Vec<DerivedRow>) -> Result<()>;
    /// Delete derived rows of the given types whose `source_object` is `source` (SPEC 13.5).
    async fn delete_derived_by_source(&self, types: &[String], source: &ObjectId) -> Result<u64>;
    /// Try to take or renew a derive lease until `now + ttl` (SPEC 9.1: dedup only).
    async fn try_lease(
        &self,
        k: &DerivedKey,
        holder: &str,
        ttl: Duration,
        now: SystemTime,
    ) -> Result<bool>;
    /// Release a lease held by `holder`.
    async fn release_lease(&self, k: &DerivedKey, holder: &str) -> Result<()>;
    /// SPEC 13.2 redact: epoch+1, insert redaction rows stamped with it, objects quarantined;
    /// returns the new epoch.
    async fn redact(&self, t: RedactTxn) -> Result<u64>;
    /// Unredact quarantined ids (INVALID_ARGUMENT/not_quarantined otherwise); epoch+1.
    async fn unredact(&self, ids: &[ObjectId], audit: AuditRow) -> Result<u64>;
    /// Mark ids purged: clears inline_data, state purged (SPEC 13.2).
    async fn mark_purged(&self, ids: &[ObjectId], audit: AuditRow) -> Result<()>;
    /// Current `counters.redaction_epoch`.
    async fn redaction_epoch(&self) -> Result<u64>;
    /// Redaction rows with `epoch > since_epoch`, plus the current epoch.
    async fn list_redactions(&self, since_epoch: u64) -> Result<(Vec<RedactionRow>, u64)>;
    /// Quarantined ids whose `purge_after <= now`, at most `limit`.
    async fn due_purges(&self, now: SystemTime, limit: usize) -> Result<Vec<ObjectId>>;
    /// Append an audit row (PLAN 6.8).
    async fn append_audit(&self, row: AuditRow) -> Result<()>;
    /// The most recent audit rows, newest last, at most `limit`.
    async fn audit_log(&self, limit: usize) -> Result<Vec<AuditRow>>;
}

/// Forwarding impl so decorators can wrap shared handles.
#[async_trait]
impl<T: MetaStore + ?Sized> MetaStore for Arc<T> {
    fn repo_id(&self) -> RepoId {
        (**self).repo_id()
    }
    async fn ping(&self) -> Result<()> {
        (**self).ping().await
    }
    async fn get_objects(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ObjectRow>>> {
        (**self).get_objects(ids, c).await
    }
    async fn insert_object(&self, row: ObjectRow) -> Result<InsertOutcome> {
        (**self).insert_object(row).await
    }
    async fn insert_objects(&self, rows: Vec<ObjectRow>) -> Result<Vec<InsertOutcome>> {
        (**self).insert_objects(rows).await
    }
    async fn record_upload(&self, id: &ObjectId, principal: &str, at: SystemTime) -> Result<()> {
        (**self).record_upload(id, principal, at).await
    }
    async fn record_uploads(
        &self,
        ids: &[ObjectId],
        principal: &str,
        at: SystemTime,
    ) -> Result<()> {
        (**self).record_uploads(ids, principal, at).await
    }
    async fn has_upload(&self, id: &ObjectId, principal: &str) -> Result<bool> {
        (**self).has_upload(id, principal).await
    }
    async fn has_uploads(&self, ids: &[ObjectId], principal: &str) -> Result<Vec<bool>> {
        (**self).has_uploads(ids, principal).await
    }
    async fn object_paths(&self, id: &ObjectId, limit: usize) -> Result<Vec<ObjectPathRow>> {
        (**self).object_paths(id, limit).await
    }
    async fn object_paths_many(
        &self,
        ids: &[ObjectId],
        limit: usize,
    ) -> Result<Vec<Vec<ObjectPathRow>>> {
        (**self).object_paths_many(ids, limit).await
    }
    async fn get_changesets(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ChangesetRow>>> {
        (**self).get_changesets(ids, c).await
    }
    async fn changesets_by_prefix(&self, hex_prefix: &str, limit: usize) -> Result<Vec<ObjectId>> {
        (**self).changesets_by_prefix(hex_prefix, limit).await
    }
    async fn children(&self, parent: &ObjectId) -> Result<Vec<ObjectId>> {
        (**self).children(parent).await
    }
    async fn has_root_changeset(&self) -> Result<bool> {
        (**self).has_root_changeset().await
    }
    async fn commit_changeset(&self, b: ChangesetBundle) -> Result<InsertOutcome> {
        (**self).commit_changeset(b).await
    }
    async fn get_ref(&self, name: &RefName) -> Result<Option<RefRow>> {
        (**self).get_ref(name).await
    }
    async fn list_refs(&self) -> Result<Vec<RefRow>> {
        (**self).list_refs().await
    }
    async fn cas_ref(&self, cas: RefCas) -> Result<CasOutcome> {
        (**self).cas_ref(cas).await
    }
    async fn ref_log(
        &self,
        name: &RefName,
        from_version: u64,
        limit: usize,
    ) -> Result<Vec<RefLogRow>> {
        (**self).ref_log(name, from_version, limit).await
    }
    async fn get_landed(&self, name: &RefName, change_id: &ChangeId) -> Result<Option<LandedRow>> {
        (**self).get_landed(name, change_id).await
    }
    async fn get_derived(&self, k: &DerivedKey, c: Consistency) -> Result<Option<DerivedRow>> {
        (**self).get_derived(k, c).await
    }
    async fn put_derived(&self, rows: Vec<DerivedRow>) -> Result<()> {
        (**self).put_derived(rows).await
    }
    async fn delete_derived_by_source(&self, types: &[String], source: &ObjectId) -> Result<u64> {
        (**self).delete_derived_by_source(types, source).await
    }
    async fn try_lease(
        &self,
        k: &DerivedKey,
        holder: &str,
        ttl: Duration,
        now: SystemTime,
    ) -> Result<bool> {
        (**self).try_lease(k, holder, ttl, now).await
    }
    async fn release_lease(&self, k: &DerivedKey, holder: &str) -> Result<()> {
        (**self).release_lease(k, holder).await
    }
    async fn redact(&self, t: RedactTxn) -> Result<u64> {
        (**self).redact(t).await
    }
    async fn unredact(&self, ids: &[ObjectId], audit: AuditRow) -> Result<u64> {
        (**self).unredact(ids, audit).await
    }
    async fn mark_purged(&self, ids: &[ObjectId], audit: AuditRow) -> Result<()> {
        (**self).mark_purged(ids, audit).await
    }
    async fn redaction_epoch(&self) -> Result<u64> {
        (**self).redaction_epoch().await
    }
    async fn list_redactions(&self, since_epoch: u64) -> Result<(Vec<RedactionRow>, u64)> {
        (**self).list_redactions(since_epoch).await
    }
    async fn due_purges(&self, now: SystemTime, limit: usize) -> Result<Vec<ObjectId>> {
        (**self).due_purges(now, limit).await
    }
    async fn append_audit(&self, row: AuditRow) -> Result<()> {
        (**self).append_audit(row).await
    }
    async fn audit_log(&self, limit: usize) -> Result<Vec<AuditRow>> {
        (**self).audit_log(limit).await
    }
}
