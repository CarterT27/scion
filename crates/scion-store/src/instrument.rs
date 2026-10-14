//! SPEC 19.0 Storage Access Counters and fault decorators.
//!
//! Object reads are counted by [`crate::ObjectRepo`] (inline trees count as `trees_read`).
//! Leases count `lease_calls` and emit a span with target [`scion_core::trace::LOCK_TARGET`].
//! Decorators check `meta.<method>.before|after` and `blob.<method>.before|after`.
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::blob::BlobStore;
use crate::meta::MetaStore;
use crate::types::*;
use async_trait::async_trait;
use bytes::Bytes;
use scion_core::{ChangeId, FaultRegistry, Kind, ObjectId, Result};
use scion_object::RefName;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, SystemTime};

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

impl Counters {
    /// A consistent-enough snapshot of every counter.
    pub fn snapshot(&self) -> CounterSnapshot {
        todo!("P03")
    }

    /// Reset every counter to zero.
    pub fn reset(&self) {
        todo!("P03")
    }

    /// Count one object read of `kind` with `bytes` raw bytes.
    pub fn record_object_read(&self, kind: Kind, bytes: u64) {
        todo!("P03")
    }

    /// Count one object write of `kind`.
    pub fn record_object_write(&self, kind: Kind) {
        todo!("P03")
    }

    /// Count one integrity failure (SPEC 16.1 class 7).
    pub fn record_integrity_failure(&self) {
        todo!("P03")
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
        todo!("P03")
    }
}

/// Counting + fault-injecting decorator over any [`MetaStore`].
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
}

/// Counting + fault-injecting decorator over any [`BlobStore`].
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
}

#[async_trait]
impl<M: MetaStore> MetaStore for InstrumentedMeta<M> {
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

#[async_trait]
impl<B: BlobStore> BlobStore for InstrumentedBlob<B> {
    async fn put(&self, key: &str, bytes: Bytes) -> Result<()> {
        todo!("P03")
    }
    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        todo!("P03")
    }
    async fn copy(&self, from: &str, to: &str) -> Result<()> {
        todo!("P03")
    }
    async fn delete(&self, key: &str) -> Result<()> {
        todo!("P03")
    }
    async fn exists(&self, key: &str) -> Result<bool> {
        todo!("P03")
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        todo!("P03")
    }
}
