//! A `MetaStore` decorator that runs every call of the wrapped store as a task on another
//! runtime (normal-sized worker stacks).
//!
//! The 256 KiB-stack test (SPEC 19.6 "without recursion depth growth") measures the engine's
//! own stack use. The in-memory backend's per-transaction stack use (persistent-map node
//! copies) is a property of the hermetic backend, not of the derivation algorithm, so it is
//! moved off the measured thread here. A production backend's work happens in its driver.

use async_trait::async_trait;
use scion_core::{ChangeId, ObjectId, Result, ScionError, subcode};
use scion_object::RefName;
use scion_store::{
    AuditRow, CasOutcome, ChangesetBundle, ChangesetRow, Consistency, DerivedKey, DerivedRow,
    InsertOutcome, LandedRow, MetaStore, ObjectPathRow, ObjectRow, RedactTxn, RedactionRow, RefCas,
    RefLogRow, RefRow, RepoId,
};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// See the module docs.
pub struct OffStackMeta {
    inner: Arc<dyn MetaStore>,
    rt: tokio::runtime::Handle,
}

impl OffStackMeta {
    pub fn new(inner: Arc<dyn MetaStore>, rt: tokio::runtime::Handle) -> Self {
        OffStackMeta { inner, rt }
    }

    async fn off<T, F>(&self, fut: F) -> Result<T>
    where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        self.rt
            .spawn(fut)
            .await
            .map_err(|e| ScionError::unavailable(subcode::STORE_UNAVAILABLE, e.to_string()))?
    }
}

#[async_trait]
impl MetaStore for OffStackMeta {
    fn repo_id(&self) -> RepoId {
        self.inner.repo_id()
    }
    async fn ping(&self) -> Result<()> {
        let m = self.inner.clone();
        self.off(async move { m.ping().await }).await
    }
    async fn get_objects(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ObjectRow>>> {
        let (m, ids) = (self.inner.clone(), ids.to_vec());
        self.off(async move { m.get_objects(&ids, c).await }).await
    }
    async fn insert_object(&self, row: ObjectRow) -> Result<InsertOutcome> {
        let m = self.inner.clone();
        self.off(async move { m.insert_object(row).await }).await
    }
    async fn record_upload(&self, id: &ObjectId, principal: &str, at: SystemTime) -> Result<()> {
        let (m, id, p) = (self.inner.clone(), *id, principal.to_owned());
        self.off(async move { m.record_upload(&id, &p, at).await })
            .await
    }
    async fn record_uploads(
        &self,
        ids: &[ObjectId],
        principal: &str,
        at: SystemTime,
    ) -> Result<()> {
        let (m, ids, p) = (self.inner.clone(), ids.to_vec(), principal.to_owned());
        self.off(async move { m.record_uploads(&ids, &p, at).await })
            .await
    }
    async fn has_upload(&self, id: &ObjectId, principal: &str) -> Result<bool> {
        let (m, id, p) = (self.inner.clone(), *id, principal.to_owned());
        self.off(async move { m.has_upload(&id, &p).await }).await
    }
    async fn has_uploads(&self, ids: &[ObjectId], principal: &str) -> Result<Vec<bool>> {
        let (m, ids, p) = (self.inner.clone(), ids.to_vec(), principal.to_owned());
        self.off(async move { m.has_uploads(&ids, &p).await }).await
    }
    async fn object_paths(&self, id: &ObjectId, limit: usize) -> Result<Vec<ObjectPathRow>> {
        let (m, id) = (self.inner.clone(), *id);
        self.off(async move { m.object_paths(&id, limit).await })
            .await
    }
    async fn get_changesets(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ChangesetRow>>> {
        let (m, ids) = (self.inner.clone(), ids.to_vec());
        self.off(async move { m.get_changesets(&ids, c).await })
            .await
    }
    async fn changesets_by_prefix(&self, hex_prefix: &str, limit: usize) -> Result<Vec<ObjectId>> {
        let (m, p) = (self.inner.clone(), hex_prefix.to_owned());
        self.off(async move { m.changesets_by_prefix(&p, limit).await })
            .await
    }
    async fn children(&self, parent: &ObjectId) -> Result<Vec<ObjectId>> {
        let (m, p) = (self.inner.clone(), *parent);
        self.off(async move { m.children(&p).await }).await
    }
    async fn has_root_changeset(&self) -> Result<bool> {
        let m = self.inner.clone();
        self.off(async move { m.has_root_changeset().await }).await
    }
    async fn commit_changeset(&self, b: ChangesetBundle) -> Result<InsertOutcome> {
        let m = self.inner.clone();
        self.off(async move { m.commit_changeset(b).await }).await
    }
    async fn get_ref(&self, name: &RefName) -> Result<Option<RefRow>> {
        let (m, n) = (self.inner.clone(), name.clone());
        self.off(async move { m.get_ref(&n).await }).await
    }
    async fn list_refs(&self) -> Result<Vec<RefRow>> {
        let m = self.inner.clone();
        self.off(async move { m.list_refs().await }).await
    }
    async fn cas_ref(&self, cas: RefCas) -> Result<CasOutcome> {
        let m = self.inner.clone();
        self.off(async move { m.cas_ref(cas).await }).await
    }
    async fn ref_log(
        &self,
        name: &RefName,
        from_version: u64,
        limit: usize,
    ) -> Result<Vec<RefLogRow>> {
        let (m, n) = (self.inner.clone(), name.clone());
        self.off(async move { m.ref_log(&n, from_version, limit).await })
            .await
    }
    async fn get_landed(&self, name: &RefName, change_id: &ChangeId) -> Result<Option<LandedRow>> {
        let (m, n, c) = (self.inner.clone(), name.clone(), *change_id);
        self.off(async move { m.get_landed(&n, &c).await }).await
    }
    async fn get_derived(&self, k: &DerivedKey, c: Consistency) -> Result<Option<DerivedRow>> {
        let (m, k) = (self.inner.clone(), k.clone());
        self.off(async move { m.get_derived(&k, c).await }).await
    }
    async fn put_derived(&self, rows: Vec<DerivedRow>) -> Result<()> {
        let m = self.inner.clone();
        self.off(async move { m.put_derived(rows).await }).await
    }
    async fn delete_derived_by_source(&self, types: &[String], source: &ObjectId) -> Result<u64> {
        let (m, t, s) = (self.inner.clone(), types.to_vec(), *source);
        self.off(async move { m.delete_derived_by_source(&t, &s).await })
            .await
    }
    async fn try_lease(
        &self,
        k: &DerivedKey,
        holder: &str,
        ttl: Duration,
        now: SystemTime,
    ) -> Result<bool> {
        let (m, k, h) = (self.inner.clone(), k.clone(), holder.to_owned());
        self.off(async move { m.try_lease(&k, &h, ttl, now).await })
            .await
    }
    async fn release_lease(&self, k: &DerivedKey, holder: &str) -> Result<()> {
        let (m, k, h) = (self.inner.clone(), k.clone(), holder.to_owned());
        self.off(async move { m.release_lease(&k, &h).await }).await
    }
    async fn redact(&self, t: RedactTxn) -> Result<u64> {
        let m = self.inner.clone();
        self.off(async move { m.redact(t).await }).await
    }
    async fn unredact(&self, ids: &[ObjectId], audit: AuditRow) -> Result<u64> {
        let (m, ids) = (self.inner.clone(), ids.to_vec());
        self.off(async move { m.unredact(&ids, audit).await }).await
    }
    async fn mark_purged(&self, ids: &[ObjectId], audit: AuditRow) -> Result<()> {
        let (m, ids) = (self.inner.clone(), ids.to_vec());
        self.off(async move { m.mark_purged(&ids, audit).await })
            .await
    }
    async fn redaction_epoch(&self) -> Result<u64> {
        let m = self.inner.clone();
        self.off(async move { m.redaction_epoch().await }).await
    }
    async fn list_redactions(&self, since_epoch: u64) -> Result<(Vec<RedactionRow>, u64)> {
        let m = self.inner.clone();
        self.off(async move { m.list_redactions(since_epoch).await })
            .await
    }
    async fn due_purges(&self, now: SystemTime, limit: usize) -> Result<Vec<ObjectId>> {
        let m = self.inner.clone();
        self.off(async move { m.due_purges(now, limit).await })
            .await
    }
    async fn append_audit(&self, row: AuditRow) -> Result<()> {
        let m = self.inner.clone();
        self.off(async move { m.append_audit(row).await }).await
    }
    async fn audit_log(&self, limit: usize) -> Result<Vec<AuditRow>> {
        let m = self.inner.clone();
        self.off(async move { m.audit_log(limit).await }).await
    }
}
