//! SPEC 19.0 Storage Access Counters and fault decorators.
//!
//! Object reads are counted by [`crate::ObjectRepo`] (inline trees count as `trees_read`).
//! Leases count `lease_calls` and emit a span with target [`scion_core::trace::LOCK_TARGET`].
//! Decorators check `meta.<method>.before|after` and `blob.<method>.before|after`:
//!
//! - `before` is checked before the inner call; an armed `Error` / `Crash` fails the call with no
//!   effect.
//! - `after` is checked once the inner call succeeded: an armed `Error` / `Crash` (or a latched
//!   crash) fails the call after its effect, and `FailAfter` returns UNAVAILABLE/`injected`
//!   after the effect ("committed, response lost").

use crate::blob::BlobStore;
use crate::meta::MetaStore;
use crate::types::*;
use async_trait::async_trait;
use bytes::Bytes;
use scion_core::{ChangeId, FaultRegistry, Kind, ObjectId, Result, trace::LOCK_TARGET};
use scion_object::RefName;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tracing::Instrument;

/// Atomic storage access counters (SPEC 19.0, 19.18).
#[derive(Default, Debug)]
pub struct Counters {
    meta_txns: AtomicU64,
    meta_rows_read: AtomicU64,
    object_paths_rows_read: AtomicU64,
    lease_calls: AtomicU64,
    blob_gets: AtomicU64,
    blob_puts: AtomicU64,
    blob_bytes_read: AtomicU64,
    objects_read_by_kind: [AtomicU64; 5],
    trees_written: AtomicU64,
    objects_written: AtomicU64,
    object_bytes_read: AtomicU64,
    integrity_failures: AtomicU64,
}

fn load(a: &AtomicU64) -> u64 {
    a.load(Ordering::SeqCst)
}

fn bump(a: &AtomicU64, n: u64) {
    a.fetch_add(n, Ordering::SeqCst);
}

impl Counters {
    /// A consistent-enough snapshot of every counter.
    pub fn snapshot(&self) -> CounterSnapshot {
        let by_kind = |k: Kind| load(&self.objects_read_by_kind[k.index()]);
        CounterSnapshot {
            meta_txns: load(&self.meta_txns),
            meta_rows_read: load(&self.meta_rows_read),
            object_paths_rows_read: load(&self.object_paths_rows_read),
            lease_calls: load(&self.lease_calls),
            blob_gets: load(&self.blob_gets),
            blob_puts: load(&self.blob_puts),
            blob_bytes_read: load(&self.blob_bytes_read),
            trees_read: by_kind(Kind::Tree),
            blobs_read: by_kind(Kind::Blob),
            changesets_read: by_kind(Kind::Changeset),
            metas_read: by_kind(Kind::Meta),
            derived_read: by_kind(Kind::Derived(0x10)),
            trees_written: load(&self.trees_written),
            objects_written: load(&self.objects_written),
            object_bytes_read: load(&self.object_bytes_read),
            integrity_failures: load(&self.integrity_failures),
        }
    }

    /// Reset every counter to zero.
    pub fn reset(&self) {
        let all = [
            &self.meta_txns,
            &self.meta_rows_read,
            &self.object_paths_rows_read,
            &self.lease_calls,
            &self.blob_gets,
            &self.blob_puts,
            &self.blob_bytes_read,
            &self.trees_written,
            &self.objects_written,
            &self.object_bytes_read,
            &self.integrity_failures,
        ];
        for a in all.into_iter().chain(self.objects_read_by_kind.iter()) {
            a.store(0, Ordering::SeqCst);
        }
    }

    /// Count one object read of `kind` with `bytes` raw bytes.
    pub fn record_object_read(&self, kind: Kind, bytes: u64) {
        bump(&self.objects_read_by_kind[kind.index()], 1);
        bump(&self.object_bytes_read, bytes);
    }

    /// Count one object write of `kind`.
    pub fn record_object_write(&self, kind: Kind) {
        bump(&self.objects_written, 1);
        if kind == Kind::Tree {
            bump(&self.trees_written, 1);
        }
    }

    /// Count one integrity failure (SPEC 16.1 class 7).
    pub fn record_integrity_failure(&self) {
        bump(&self.integrity_failures, 1);
    }
}

/// A snapshot of [`Counters`]; subtract two snapshots for a delta.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CounterSnapshot {
    /// Metadata transactions.
    pub meta_txns: u64,
    /// Metadata rows read.
    pub meta_rows_read: u64,
    /// `object_paths` rows read.
    pub object_paths_rows_read: u64,
    /// Lease calls (try/release).
    pub lease_calls: u64,
    /// Object storage gets.
    pub blob_gets: u64,
    /// Object storage puts.
    pub blob_puts: u64,
    /// Object storage bytes read.
    pub blob_bytes_read: u64,
    /// Trees read (inline or not).
    pub trees_read: u64,
    /// Blobs read.
    pub blobs_read: u64,
    /// Changesets read.
    pub changesets_read: u64,
    /// Metas read.
    pub metas_read: u64,
    /// Derived objects read.
    pub derived_read: u64,
    /// Trees written.
    pub trees_written: u64,
    /// Objects written (any kind).
    pub objects_written: u64,
    /// Raw object bytes read.
    pub object_bytes_read: u64,
    /// Integrity failures detected.
    pub integrity_failures: u64,
}

impl std::ops::Sub for CounterSnapshot {
    type Output = CounterSnapshot;

    fn sub(self, rhs: CounterSnapshot) -> CounterSnapshot {
        CounterSnapshot {
            meta_txns: self.meta_txns.saturating_sub(rhs.meta_txns),
            meta_rows_read: self.meta_rows_read.saturating_sub(rhs.meta_rows_read),
            object_paths_rows_read: self
                .object_paths_rows_read
                .saturating_sub(rhs.object_paths_rows_read),
            lease_calls: self.lease_calls.saturating_sub(rhs.lease_calls),
            blob_gets: self.blob_gets.saturating_sub(rhs.blob_gets),
            blob_puts: self.blob_puts.saturating_sub(rhs.blob_puts),
            blob_bytes_read: self.blob_bytes_read.saturating_sub(rhs.blob_bytes_read),
            trees_read: self.trees_read.saturating_sub(rhs.trees_read),
            blobs_read: self.blobs_read.saturating_sub(rhs.blobs_read),
            changesets_read: self.changesets_read.saturating_sub(rhs.changesets_read),
            metas_read: self.metas_read.saturating_sub(rhs.metas_read),
            derived_read: self.derived_read.saturating_sub(rhs.derived_read),
            trees_written: self.trees_written.saturating_sub(rhs.trees_written),
            objects_written: self.objects_written.saturating_sub(rhs.objects_written),
            object_bytes_read: self.object_bytes_read.saturating_sub(rhs.object_bytes_read),
            integrity_failures: self
                .integrity_failures
                .saturating_sub(rhs.integrity_failures),
        }
    }
}

/// The after-effect half of a decorated call: an armed `Error`/`Crash` at `point` (or a latched
/// crash) fails the call, and `FailAfter` returns UNAVAILABLE/`injected`.
async fn after_effect(faults: &FaultRegistry, point: &str) -> Result<()> {
    faults.check(point).await?;
    if faults.armed_after(point) {
        return Err(FaultRegistry::fail_after_error(point));
    }
    Ok(())
}

/// Runs one decorated metadata call: `meta.<name>.before`, one counted transaction, then
/// `meta.<name>.after`.
macro_rules! meta_call {
    ($self:ident, $name:literal, $call:expr) => {{
        $self
            .faults
            .check(concat!("meta.", $name, ".before"))
            .await?;
        bump(&$self.counters.meta_txns, 1);
        let out = $call.await?;
        after_effect(&$self.faults, concat!("meta.", $name, ".after")).await?;
        out
    }};
}

/// Runs one decorated object storage call: `blob.<name>.before`, the call, `blob.<name>.after`.
macro_rules! blob_call {
    ($self:ident, $name:literal, $call:expr) => {{
        $self
            .faults
            .check(concat!("blob.", $name, ".before"))
            .await?;
        let out = $call.await?;
        after_effect(&$self.faults, concat!("blob.", $name, ".after")).await?;
        out
    }};
}

/// Counting + fault-injecting decorator over any [`MetaStore`] (SPEC 19.0).
pub struct InstrumentedMeta<M: MetaStore> {
    inner: M,
    counters: Arc<Counters>,
    faults: Arc<FaultRegistry>,
}

impl<M: MetaStore> InstrumentedMeta<M> {
    /// Wrap `inner`.
    pub fn new(inner: M, c: Arc<Counters>, f: Arc<FaultRegistry>) -> Self {
        InstrumentedMeta {
            inner,
            counters: c,
            faults: f,
        }
    }

    /// The wrapped store.
    pub fn inner(&self) -> &M {
        &self.inner
    }

    fn rows(&self, n: usize) {
        bump(&self.counters.meta_rows_read, n as u64);
    }
}

/// Counting + fault-injecting decorator over any [`BlobStore`] (SPEC 19.0).
pub struct InstrumentedBlob<B: BlobStore> {
    inner: B,
    counters: Arc<Counters>,
    faults: Arc<FaultRegistry>,
}

impl<B: BlobStore> InstrumentedBlob<B> {
    /// Wrap `inner`.
    pub fn new(inner: B, c: Arc<Counters>, f: Arc<FaultRegistry>) -> Self {
        InstrumentedBlob {
            inner,
            counters: c,
            faults: f,
        }
    }

    /// The wrapped store.
    pub fn inner(&self) -> &B {
        &self.inner
    }
}

fn some_count<T>(v: &[Option<T>]) -> usize {
    v.iter().filter(|x| x.is_some()).count()
}

#[async_trait]
impl<M: MetaStore> MetaStore for InstrumentedMeta<M> {
    fn repo_id(&self) -> RepoId {
        self.inner.repo_id()
    }
    async fn ping(&self) -> Result<()> {
        meta_call!(self, "ping", self.inner.ping());
        Ok(())
    }
    async fn get_objects(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ObjectRow>>> {
        let out = meta_call!(self, "get_objects", self.inner.get_objects(ids, c));
        self.rows(some_count(&out));
        Ok(out)
    }
    async fn insert_object(&self, row: ObjectRow) -> Result<InsertOutcome> {
        Ok(meta_call!(
            self,
            "insert_object",
            self.inner.insert_object(row)
        ))
    }
    async fn record_upload(&self, id: &ObjectId, principal: &str, at: SystemTime) -> Result<()> {
        meta_call!(
            self,
            "record_upload",
            self.inner.record_upload(id, principal, at)
        );
        Ok(())
    }
    async fn record_uploads(
        &self,
        ids: &[ObjectId],
        principal: &str,
        at: SystemTime,
    ) -> Result<()> {
        // Keep the existing ownership-write fault checkpoints for batched writes too.
        meta_call!(
            self,
            "record_upload",
            self.inner.record_uploads(ids, principal, at)
        );
        Ok(())
    }
    async fn has_upload(&self, id: &ObjectId, principal: &str) -> Result<bool> {
        let out = meta_call!(self, "has_upload", self.inner.has_upload(id, principal));
        self.rows(usize::from(out));
        Ok(out)
    }
    async fn has_uploads(&self, ids: &[ObjectId], principal: &str) -> Result<Vec<bool>> {
        let out = meta_call!(self, "has_uploads", self.inner.has_uploads(ids, principal));
        self.rows(out.iter().filter(|present| **present).count());
        Ok(out)
    }
    async fn object_paths(&self, id: &ObjectId, limit: usize) -> Result<Vec<ObjectPathRow>> {
        let out = meta_call!(self, "object_paths", self.inner.object_paths(id, limit));
        self.rows(out.len());
        bump(&self.counters.object_paths_rows_read, out.len() as u64);
        Ok(out)
    }
    async fn object_paths_many(
        &self,
        ids: &[ObjectId],
        limit: usize,
    ) -> Result<Vec<Vec<ObjectPathRow>>> {
        let out = meta_call!(
            self,
            "object_paths_many",
            self.inner.object_paths_many(ids, limit)
        );
        let n: usize = out.iter().map(Vec::len).sum();
        self.rows(n);
        bump(&self.counters.object_paths_rows_read, n as u64);
        Ok(out)
    }
    async fn get_changesets(
        &self,
        ids: &[ObjectId],
        c: Consistency,
    ) -> Result<Vec<Option<ChangesetRow>>> {
        let out = meta_call!(self, "get_changesets", self.inner.get_changesets(ids, c));
        self.rows(some_count(&out));
        Ok(out)
    }
    async fn changesets_by_prefix(&self, hex_prefix: &str, limit: usize) -> Result<Vec<ObjectId>> {
        let out = meta_call!(
            self,
            "changesets_by_prefix",
            self.inner.changesets_by_prefix(hex_prefix, limit)
        );
        self.rows(out.len());
        Ok(out)
    }
    async fn children(&self, parent: &ObjectId) -> Result<Vec<ObjectId>> {
        let out = meta_call!(self, "children", self.inner.children(parent));
        self.rows(out.len());
        Ok(out)
    }
    async fn has_root_changeset(&self) -> Result<bool> {
        let out = meta_call!(self, "has_root_changeset", self.inner.has_root_changeset());
        self.rows(usize::from(out));
        Ok(out)
    }
    async fn commit_changeset(&self, b: ChangesetBundle) -> Result<InsertOutcome> {
        Ok(meta_call!(
            self,
            "commit_changeset",
            self.inner.commit_changeset(b)
        ))
    }
    async fn get_ref(&self, name: &RefName) -> Result<Option<RefRow>> {
        let out = meta_call!(self, "get_ref", self.inner.get_ref(name));
        self.rows(usize::from(out.is_some()));
        Ok(out)
    }
    async fn list_refs(&self) -> Result<Vec<RefRow>> {
        let out = meta_call!(self, "list_refs", self.inner.list_refs());
        self.rows(out.len());
        Ok(out)
    }
    async fn cas_ref(&self, cas: RefCas) -> Result<CasOutcome> {
        let out = meta_call!(self, "cas_ref", self.inner.cas_ref(cas));
        self.rows(1);
        Ok(out)
    }
    async fn ref_log(
        &self,
        name: &RefName,
        from_version: u64,
        limit: usize,
    ) -> Result<Vec<RefLogRow>> {
        let out = meta_call!(
            self,
            "ref_log",
            self.inner.ref_log(name, from_version, limit)
        );
        self.rows(out.len());
        Ok(out)
    }
    async fn get_landed(&self, name: &RefName, change_id: &ChangeId) -> Result<Option<LandedRow>> {
        let out = meta_call!(self, "get_landed", self.inner.get_landed(name, change_id));
        self.rows(usize::from(out.is_some()));
        Ok(out)
    }
    async fn get_derived(&self, k: &DerivedKey, c: Consistency) -> Result<Option<DerivedRow>> {
        let out = meta_call!(self, "get_derived", self.inner.get_derived(k, c));
        self.rows(usize::from(out.is_some()));
        Ok(out)
    }
    async fn put_derived(&self, rows: Vec<DerivedRow>) -> Result<()> {
        meta_call!(self, "put_derived", self.inner.put_derived(rows));
        Ok(())
    }
    async fn delete_derived_by_source(&self, types: &[String], source: &ObjectId) -> Result<u64> {
        Ok(meta_call!(
            self,
            "delete_derived_by_source",
            self.inner.delete_derived_by_source(types, source)
        ))
    }
    async fn try_lease(
        &self,
        k: &DerivedKey,
        holder: &str,
        ttl: Duration,
        now: SystemTime,
    ) -> Result<bool> {
        bump(&self.counters.lease_calls, 1);
        let span = tracing::info_span!(
            target: LOCK_TARGET,
            "derive_lease",
            op = "try",
            ty = %k.ty,
            version = k.version,
            holder = holder
        );
        async move {
            let got = meta_call!(self, "try_lease", self.inner.try_lease(k, holder, ttl, now));
            tracing::debug!(target: LOCK_TARGET, acquired = got, "derive lease attempt");
            Ok(got)
        }
        .instrument(span)
        .await
    }
    async fn release_lease(&self, k: &DerivedKey, holder: &str) -> Result<()> {
        bump(&self.counters.lease_calls, 1);
        let span = tracing::info_span!(
            target: LOCK_TARGET,
            "derive_lease",
            op = "release",
            ty = %k.ty,
            version = k.version,
            holder = holder
        );
        async move {
            meta_call!(self, "release_lease", self.inner.release_lease(k, holder));
            Ok(())
        }
        .instrument(span)
        .await
    }
    async fn redact(&self, t: RedactTxn) -> Result<u64> {
        Ok(meta_call!(self, "redact", self.inner.redact(t)))
    }
    async fn unredact(&self, ids: &[ObjectId], audit: AuditRow) -> Result<u64> {
        Ok(meta_call!(
            self,
            "unredact",
            self.inner.unredact(ids, audit)
        ))
    }
    async fn mark_purged(&self, ids: &[ObjectId], audit: AuditRow) -> Result<()> {
        meta_call!(self, "mark_purged", self.inner.mark_purged(ids, audit));
        Ok(())
    }
    async fn redaction_epoch(&self) -> Result<u64> {
        let out = meta_call!(self, "redaction_epoch", self.inner.redaction_epoch());
        self.rows(1);
        Ok(out)
    }
    async fn list_redactions(&self, since_epoch: u64) -> Result<(Vec<RedactionRow>, u64)> {
        let out = meta_call!(
            self,
            "list_redactions",
            self.inner.list_redactions(since_epoch)
        );
        self.rows(out.0.len() + 1);
        Ok(out)
    }
    async fn due_purges(&self, now: SystemTime, limit: usize) -> Result<Vec<ObjectId>> {
        let out = meta_call!(self, "due_purges", self.inner.due_purges(now, limit));
        self.rows(out.len());
        Ok(out)
    }
    async fn append_audit(&self, row: AuditRow) -> Result<()> {
        meta_call!(self, "append_audit", self.inner.append_audit(row));
        Ok(())
    }
    async fn audit_log(&self, limit: usize) -> Result<Vec<AuditRow>> {
        let out = meta_call!(self, "audit_log", self.inner.audit_log(limit));
        self.rows(out.len());
        Ok(out)
    }
}

#[async_trait]
impl<B: BlobStore> BlobStore for InstrumentedBlob<B> {
    async fn put(&self, key: &str, bytes: Bytes) -> Result<()> {
        self.faults.check("blob.put.before").await?;
        bump(&self.counters.blob_puts, 1);
        self.inner.put(key, bytes).await?;
        after_effect(&self.faults, "blob.put.after").await
    }
    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        self.faults.check("blob.get.before").await?;
        bump(&self.counters.blob_gets, 1);
        let out = self.inner.get(key).await?;
        if let Some(b) = &out {
            bump(&self.counters.blob_bytes_read, b.len() as u64);
        }
        after_effect(&self.faults, "blob.get.after").await?;
        Ok(out)
    }
    async fn copy(&self, from: &str, to: &str) -> Result<()> {
        blob_call!(self, "copy", self.inner.copy(from, to));
        Ok(())
    }
    async fn delete(&self, key: &str) -> Result<()> {
        blob_call!(self, "delete", self.inner.delete(key));
        Ok(())
    }
    async fn exists(&self, key: &str) -> Result<bool> {
        Ok(blob_call!(self, "exists", self.inner.exists(key)))
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        Ok(blob_call!(self, "list", self.inner.list(prefix)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_snapshot_reset_sub() {
        let c = Counters::default();
        c.record_object_read(Kind::Tree, 10);
        c.record_object_read(Kind::Blob, 5);
        c.record_object_read(Kind::Derived(0x11), 1);
        c.record_object_write(Kind::Tree);
        c.record_object_write(Kind::Meta);
        c.record_integrity_failure();
        let a = c.snapshot();
        assert_eq!(a.trees_read, 1);
        assert_eq!(a.blobs_read, 1);
        assert_eq!(a.derived_read, 1);
        assert_eq!(a.object_bytes_read, 16);
        assert_eq!(a.trees_written, 1);
        assert_eq!(a.objects_written, 2);
        assert_eq!(a.integrity_failures, 1);
        c.record_object_read(Kind::Meta, 3);
        let d = c.snapshot() - a.clone();
        assert_eq!(
            d,
            CounterSnapshot {
                metas_read: 1,
                object_bytes_read: 3,
                ..Default::default()
            }
        );
        c.reset();
        assert_eq!(c.snapshot(), CounterSnapshot::default());
    }
}
