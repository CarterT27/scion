//! The ObjectRepo facade (SPEC 5.1, 5.3 step 1, 5.4).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::keys::object_key;
use crate::storage::Storage;
use crate::types::ChangesetRow;
use crate::types::{Area, InsertOutcome, ObjectRow};
use bytes::Bytes;
use scion_core::{Kind, ObjectId, Result};
use scion_object::{Changeset, Meta, Tree};
use std::sync::Arc;

/// ObjectRepo configuration (SPEC 6.1 `[objects]`, 6.5).
#[derive(Clone, Debug)]
pub struct ObjectRepoConfig {
    /// Key prefix (`objects.prefix`).
    pub prefix: String,
    /// Inline threshold for Tree/Meta/Derived encodings (default 65536).
    pub inline_threshold_bytes: usize,
    /// zstd level (default 3).
    pub zstd_level: i32,
    /// `limits.max_blob_bytes`.
    pub max_blob_bytes: u64,
    /// Capacity of each decoded-object LRU.
    pub decoded_cache_entries: usize,
}

impl Default for ObjectRepoConfig {
    fn default() -> Self {
        ObjectRepoConfig {
            prefix: "scion".to_owned(),
            inline_threshold_bytes: 65_536,
            zstd_level: 3,
            max_blob_bytes: 104_857_600,
            decoded_cache_entries: 10_000,
        }
    }
}

/// An object exactly as stored.
#[derive(Clone, Debug)]
pub enum StoredFrame {
    /// The zstd frame exactly as in object storage.
    Zstd(Bytes),
    /// Inline data (uncompressed canonical bytes).
    Raw(Bytes),
}

/// The object facade: write ordering, inline threshold, verification, stale-then-strong reads
/// and decoded LRU caches.
#[derive(Clone)]
pub struct ObjectRepo {
    storage: Storage,
    cfg: Arc<ObjectRepoConfig>,
}

impl ObjectRepo {
    /// A facade over `storage` (no IO).
    pub fn new(storage: Storage, cfg: ObjectRepoConfig) -> Self {
        ObjectRepo {
            storage,
            cfg: Arc::new(cfg),
        }
    }

    /// The underlying storage bundle.
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// The configuration.
    pub fn config(&self) -> &ObjectRepoConfig {
        &self.cfg
    }

    /// The SPEC 5.1 key of `id` in `area`.
    pub fn key(&self, id: &ObjectId, area: Area) -> String {
        object_key(&self.cfg.prefix, self.storage.meta.repo_id(), id, area)
    }

    /// Store a blob: TOO_LARGE check, bytes first, then the row (SPEC 5.3 step 1).
    pub async fn put_blob(&self, raw: Bytes) -> Result<(ObjectId, InsertOutcome)> {
        todo!("P03")
    }

    /// Store a structured object: decode and validate known kinds; inline when at or below the
    /// threshold (Tree, Meta, Derived).
    pub async fn put_encoded(
        &self,
        kind: Kind,
        canonical: Bytes,
    ) -> Result<(ObjectId, InsertOutcome)> {
        todo!("P03")
    }

    /// Store client-supplied data under a claimed id: INTEGRITY on mismatch, REDACTED when the
    /// id is redacted.
    pub async fn put_verified(&self, id: &ObjectId, data: Bytes) -> Result<InsertOutcome> {
        todo!("P03")
    }

    /// The objects row: stale read first, strong retry on a miss (SPEC 5.4).
    pub async fn stat(&self, id: &ObjectId) -> Result<Option<ObjectRow>> {
        todo!("P03")
    }

    /// [`ObjectRepo::stat`] for many ids.
    pub async fn stat_many(&self, ids: &[ObjectId]) -> Result<Vec<Option<ObjectRow>>> {
        todo!("P03")
    }

    /// Raw verified bytes: NOT_FOUND | REDACTED | INTEGRITY (+ alert).
    pub async fn get(&self, id: &ObjectId) -> Result<Bytes> {
        todo!("P03")
    }

    /// Verified, then returned exactly as stored.
    pub async fn get_frame(&self, id: &ObjectId) -> Result<(StoredFrame, ObjectRow)> {
        todo!("P03")
    }

    /// Decoded tree through the LRU (immutable; survives a metadata outage).
    pub async fn get_tree(&self, id: &ObjectId) -> Result<Arc<Tree>> {
        todo!("P03")
    }

    /// Decoded Meta through the LRU.
    pub async fn get_meta(&self, id: &ObjectId) -> Result<Arc<Meta>> {
        todo!("P03")
    }

    /// Decoded changeset and its row through the LRU.
    pub async fn get_changeset(&self, id: &ObjectId) -> Result<(Arc<Changeset>, ChangesetRow)> {
        todo!("P03")
    }

    /// Copy live -> quarantine, then delete live (checkpoint `quarantine.after_copy`).
    pub async fn quarantine_bytes(&self, id: &ObjectId) -> Result<()> {
        todo!("P03")
    }

    /// Copy quarantine -> live, then delete quarantine.
    pub async fn restore_bytes(&self, id: &ObjectId) -> Result<()> {
        todo!("P03")
    }

    /// Delete both the live and quarantine keys.
    pub async fn purge_bytes(&self, id: &ObjectId) -> Result<()> {
        todo!("P03")
    }
}
