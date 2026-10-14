//! Frozen derived-data framework traits (SPEC 9.1; PLAN 2.7).

use async_trait::async_trait;
use bytes::Bytes;
use scion_core::{Kind, ObjectId, Result};
use scion_history::{ChangesetRecord, EagerInput};
use scion_object::{Meta, Tree};
use scion_store::{DerivedKey, DerivedRow};
use std::sync::Arc;
use std::time::Duration;

/// SPEC 9.1 derivation mode.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeriveMode {
    /// Computed before the changeset row is inserted.
    Eager,
    /// Computed on first query, iteratively, with write-back.
    Lazy,
}

/// SPEC 9.1 `DerivedType` descriptor.
pub trait DerivedType: Send + Sync {
    /// Stable name, e.g. `"commit_graph"`.
    fn name(&self) -> &'static str;
    /// Version; bump to re-derive (old versions stay readable).
    fn version(&self) -> u32;
    /// Eager or lazy.
    fn mode(&self) -> DeriveMode;
    /// Stores file content (affects redaction, SPEC 13.5).
    fn content_bearing(&self) -> bool;
}

/// Everything a derivation may read or write. Mockable: history and blame are tested through a
/// fake context.
#[async_trait]
pub trait DeriveCtx: Send + Sync {
    /// A decoded changeset with graph columns.
    async fn changeset(&self, id: &ObjectId) -> Result<ChangesetRecord>;
    /// A decoded tree.
    async fn tree(&self, id: &ObjectId) -> Result<Arc<Tree>>;
    /// A decoded Meta; `None` when redacted or purged.
    async fn meta(&self, id: &ObjectId) -> Result<Option<Arc<Meta>>>;
    /// Blob content; redacted or purged content reads as empty (SPEC 13.5).
    async fn blob_or_empty(&self, id: &ObjectId) -> Result<Bytes>;
    /// Raw verified bytes of any object.
    async fn get_object(&self, id: &ObjectId) -> Result<Bytes>;
    /// Store a structured object; returns its id.
    async fn put_object(&self, kind: Kind, canonical: Bytes) -> Result<ObjectId>;
    /// A stored derived value.
    async fn stored(&self, key: &DerivedKey) -> Result<Option<Bytes>>;
}

/// An eager derived type (SPEC 9.1).
#[async_trait]
pub trait EagerType: DerivedType {
    /// Rows to insert with the changeset (deterministic).
    async fn derive(&self, cx: &dyn DeriveCtx, input: &EagerInput<'_>) -> Result<Vec<DerivedRow>>;
}

/// A lazy derived type (SPEC 9.1): derived iteratively from the nearest stored dependencies.
#[async_trait]
pub trait LazyType: DerivedType {
    /// Keys of the same type that must be derived first.
    async fn deps(&self, cx: &dyn DeriveCtx, key: &[u8]) -> Result<Vec<Bytes>>;
    /// Derive the value for `key` from its dependencies' `(key, value)` pairs (deterministic).
    async fn derive(
        &self,
        cx: &dyn DeriveCtx,
        key: &[u8],
        deps: &[(Bytes, Bytes)],
    ) -> Result<Bytes>;
}

/// Engine configuration (SPEC 6.1 `[derive]`).
#[derive(Clone, Debug)]
pub struct DeriveConfig {
    /// Lease TTL (default 30 s).
    pub lease_ttl: Duration,
    /// Lazy worker count (default 16).
    pub lazy_workers: usize,
    /// Lease holder id of this process.
    pub holder_id: String,
}

impl Default for DeriveConfig {
    fn default() -> Self {
        DeriveConfig {
            lease_ttl: Duration::from_millis(30_000),
            lazy_workers: 16,
            holder_id: "scion".to_owned(),
        }
    }
}

/// Metrics hook (SPEC 15.2 `scion_derive_duration_seconds`, `scion_derive_backlog`); wired by
/// scion-server so this crate needs no scion-obs dependency.
pub trait DeriveMetrics: Send + Sync {
    /// Record one derivation duration.
    fn derive_duration(&self, ty: &str, mode: DeriveMode, d: Duration);
    /// Adjust the backlog gauge of `ty` by `delta`.
    fn backlog(&self, ty: &str, delta: i64);
}
