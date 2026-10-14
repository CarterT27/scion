//! The ObjectRepo facade (SPEC 5.1, 5.3 step 1, 5.4; `docs/contracts/storage-semantics.md` 7).
//!
//! - Writes validate size and the decoded form of known kinds, reject redacted ids, then write
//!   the bytes (object storage or inline) BEFORE the `objects` row, so a live row always implies
//!   durable bytes.
//! - Reads verify BLAKE3 of the decompressed content against the id; a mismatch is an integrity
//!   failure (counter, ERROR event `integrity_alert`, INTEGRITY; PLAN 6.10).
//! - Immutable rows are read stale first and retried strongly on a miss (SPEC 5.4).
//! - Decoded trees, metas and changesets are kept in bounded LRUs, so immutable reads keep
//!   working while the metadata store is down (SPEC 16.1 class 1).

use crate::keys::object_key;
use crate::storage::Storage;
use crate::types::{Area, ChangesetRow, Consistency, InsertOutcome, ObjectRow, ObjectState};
use bytes::Bytes;
use lru::LruCache;
use parking_lot::Mutex;
use scion_core::{
    Clock, ErrorCode, Kind, ObjectId, Result, ScionError, SystemClock, points, subcode,
};
use scion_object::{Canonical, Changeset, HistoryNode, Meta, Tree};
use std::num::NonZeroUsize;
use std::sync::Arc;

/// Payloads above this size are (de)compressed on the blocking thread pool.
const BLOCKING_THRESHOLD: usize = 1 << 20;

/// Derived kind of history nodes (SPEC 9.2), the only derived kind with a known decoded form.
const HISTORY_NODE_KIND: u8 = 0x10;

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

/// Bounded LRUs of decoded immutable objects (value: decoded object plus raw byte length).
struct Caches {
    trees: Mutex<LruCache<ObjectId, (Arc<Tree>, u64)>>,
    metas: Mutex<LruCache<ObjectId, (Arc<Meta>, u64)>>,
    changesets: Mutex<LruCache<ObjectId, (Arc<Changeset>, ChangesetRow)>>,
}

impl Caches {
    fn new(entries: usize) -> Self {
        let cap = NonZeroUsize::new(entries).unwrap_or(NonZeroUsize::MIN);
        Caches {
            trees: Mutex::new(LruCache::new(cap)),
            metas: Mutex::new(LruCache::new(cap)),
            changesets: Mutex::new(LruCache::new(cap)),
        }
    }
}

/// The object facade: write ordering, inline threshold, verification, stale-then-strong reads
/// and decoded LRU caches.
#[derive(Clone)]
pub struct ObjectRepo {
    storage: Storage,
    cfg: Arc<ObjectRepoConfig>,
    caches: Arc<Caches>,
    clock: Arc<dyn Clock>,
}

fn store_error(msg: impl Into<String>) -> ScionError {
    ScionError::unavailable(subcode::STORE_UNAVAILABLE, msg)
}

fn not_found(id: &ObjectId) -> ScionError {
    ScionError::not_found(format!("object {id} not found"))
}

fn invalid_kind(msg: impl Into<String>) -> ScionError {
    ScionError::invalid(subcode::INVALID_KIND, msg)
}

/// True when `raw` hashes to `id` (SPEC 4.2: blobs over raw content, other kinds over the
/// canonical encoding).
fn digest_matches(id: &ObjectId, raw: &[u8]) -> bool {
    let computed = match id.kind() {
        Kind::Blob => ObjectId::hash_blob(raw),
        k => ObjectId::hash_encoded(k, raw),
    };
    computed == *id
}

/// [`digest_matches`] off the async worker for large payloads (hashing 100 MiB takes tens of
/// milliseconds).
async fn verify_digest(id: &ObjectId, raw: &Bytes) -> Result<bool> {
    let (id, raw) = (*id, raw.clone());
    maybe_blocking(raw.len(), move || digest_matches(&id, &raw)).await
}

/// Run `f` inline for small payloads and on the blocking pool for large ones.
async fn maybe_blocking<T: Send + 'static>(
    len: usize,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T> {
    if len <= BLOCKING_THRESHOLD {
        return Ok(f());
    }
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| store_error(format!("blocking task failed: {e}")))
}

/// Decompress a stored zstd frame (one or more concatenated frames), bounded by the row size.
async fn decompress(frame: Bytes, size: u64) -> Option<Bytes> {
    let cap = usize::try_from(size).ok()?;
    let len = frame.len();
    maybe_blocking(len.max(cap), move || {
        zstd::bulk::decompress(&frame, cap).ok().map(Bytes::from)
    })
    .await
    .ok()
    .flatten()
    .filter(|b| b.len() == cap)
}

/// A synthesized objects row for a changeset (changesets live only in `changesets.encoded`).
fn changeset_object_row(row: &ChangesetRow) -> ObjectRow {
    ObjectRow {
        id: row.id,
        kind: Kind::Changeset,
        size: row.encoded.len() as u64,
        stored_size: row.encoded.len() as u64,
        inline_data: Some(row.encoded.clone()),
        state: ObjectState::Live,
        created_at: row.created_at,
    }
}

impl ObjectRepo {
    /// A facade over `storage` (no IO).
    pub fn new(storage: Storage, cfg: ObjectRepoConfig) -> Self {
        let caches = Arc::new(Caches::new(cfg.decoded_cache_entries));
        ObjectRepo {
            storage,
            cfg: Arc::new(cfg),
            caches,
            clock: Arc::new(SystemClock),
        }
    }

    /// Use `clock` for `objects.created_at` (PLAN 5.1: time comes only from `Clock`).
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
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

    /// Drop `id` from every decoded-object LRU (SPEC 13.2: redaction tooling may call it after
    /// redacting a Meta).
    pub fn evict_cached(&self, id: &ObjectId) {
        self.caches.trees.lock().pop(id);
        self.caches.metas.lock().pop(id);
        self.caches.changesets.lock().pop(id);
    }

    fn check_size(&self, len: usize) -> Result<()> {
        let actual = len as u64;
        if actual > self.cfg.max_blob_bytes {
            return Err(ScionError::too_large(
                "max_blob_bytes",
                self.cfg.max_blob_bytes,
                actual,
            ));
        }
        Ok(())
    }

    /// SPEC 17: reject any object whose decoded form violates SPEC 4 before persisting.
    fn validate_decoded(kind: Kind, canonical: &[u8]) -> Result<()> {
        match kind {
            Kind::Tree => Tree::decode(canonical).map(|_| ()),
            Kind::Meta => Meta::decode(canonical).map(|_| ()),
            Kind::Derived(HISTORY_NODE_KIND) => HistoryNode::decode(canonical).map(|_| ()),
            Kind::Derived(_) => Ok(()),
            Kind::Changeset => Err(invalid_kind(
                "changesets are stored only by CreateChangeset, not as objects",
            )),
            Kind::Blob => Err(invalid_kind("blobs are stored with put_blob")),
        }
    }

    fn integrity_alert(&self, id: &ObjectId) -> ScionError {
        self.storage.counters.record_integrity_failure();
        tracing::error!(
            event = "integrity_alert",
            object = %id,
            tier = "server",
            "stored object bytes do not match their identifier"
        );
        ScionError::integrity(id)
    }

    /// The common SPEC 5.3 step 1 write path for a validated object.
    async fn store(&self, id: ObjectId, raw: Bytes) -> Result<InsertOutcome> {
        let kind = id.kind();
        // Re-introduction blocking reads redaction state, so it must not use a stale read
        // (SPEC 5.4, 13.6): a stale `live` row could hide a fresh redaction.
        let existing = self
            .storage
            .meta
            .get_objects(&[id], Consistency::Strong)
            .await?
            .pop()
            .flatten();
        if let Some(row) = existing {
            return match row.state {
                ObjectState::Live => Ok(InsertOutcome::AlreadyExists),
                ObjectState::Quarantined | ObjectState::Purged => Err(ScionError::redacted(&id)),
            };
        }
        let inline = kind != Kind::Blob && raw.len() <= self.cfg.inline_threshold_bytes;
        let size = raw.len() as u64;
        let (inline_data, stored_size) = if inline {
            (Some(raw), size)
        } else {
            let level = self.cfg.zstd_level;
            let frame = maybe_blocking(raw.len(), move || zstd::bulk::compress(&raw, level))
                .await?
                .map_err(|e| store_error(format!("zstd compression failed: {e}")))?;
            let stored = frame.len() as u64;
            self.storage
                .blobs
                .put(&self.key(&id, Area::Live), Bytes::from(frame))
                .await?;
            (None, stored)
        };
        self.storage
            .faults
            .check(points::UPLOAD_AFTER_BYTES)
            .await?;
        let row = ObjectRow {
            id,
            kind,
            size,
            stored_size,
            inline_data,
            state: ObjectState::Live,
            created_at: self.clock.now(),
        };
        let outcome = self.storage.meta.insert_object(row).await?;
        self.storage.faults.check(points::UPLOAD_AFTER_ROW).await?;
        match outcome {
            InsertOutcome::Inserted => self.storage.counters.record_object_write(kind),
            InsertOutcome::AlreadyExists => {
                // Raced with another writer or a redaction: never report a redacted id as stored.
                let existing = self
                    .storage
                    .meta
                    .get_objects(&[id], Consistency::Strong)
                    .await?;
                if let Some(Some(r)) = existing.first()
                    && r.state != ObjectState::Live
                {
                    return Err(ScionError::redacted(&id));
                }
            }
        }
        Ok(outcome)
    }

    /// Store a blob: TOO_LARGE check, bytes first, then the row (SPEC 5.3 step 1).
    pub async fn put_blob(&self, raw: Bytes) -> Result<(ObjectId, InsertOutcome)> {
        self.check_size(raw.len())?;
        let id = {
            let raw = raw.clone();
            maybe_blocking(raw.len(), move || ObjectId::hash_blob(&raw)).await?
        };
        let outcome = self.store(id, raw).await?;
        Ok((id, outcome))
    }

    /// Store a structured object: decode and validate known kinds; inline when at or below the
    /// threshold (Tree, Meta, Derived). Changesets and blobs are INVALID_ARGUMENT/invalid_kind.
    pub async fn put_encoded(
        &self,
        kind: Kind,
        canonical: Bytes,
    ) -> Result<(ObjectId, InsertOutcome)> {
        self.check_size(canonical.len())?;
        Self::validate_decoded(kind, &canonical)?;
        let id = ObjectId::hash_encoded(kind, &canonical);
        let outcome = self.store(id, canonical).await?;
        Ok((id, outcome))
    }

    /// Store client-supplied data under a claimed id: INTEGRITY on mismatch, REDACTED when the
    /// id is redacted.
    pub async fn put_verified(&self, id: &ObjectId, data: Bytes) -> Result<InsertOutcome> {
        self.check_size(data.len())?;
        if id.kind() == Kind::Changeset {
            Self::validate_decoded(Kind::Changeset, &data)?;
        }
        if !verify_digest(id, &data).await? {
            return Err(ScionError::integrity(id));
        }
        if id.kind() != Kind::Blob {
            Self::validate_decoded(id.kind(), &data)?;
        }
        self.store(*id, data).await
    }

    /// The changesets row: stale read first, strong retry on a miss (SPEC 5.4).
    async fn changeset_row(&self, id: &ObjectId) -> Result<Option<ChangesetRow>> {
        let meta = &self.storage.meta;
        if let Some(Some(r)) = meta.get_changesets(&[*id], Consistency::Stale).await?.pop() {
            return Ok(Some(r));
        }
        Ok(meta
            .get_changesets(&[*id], Consistency::Strong)
            .await?
            .pop()
            .flatten())
    }

    /// The objects row: stale read first, strong retry on a miss (SPEC 5.4). Changeset ids
    /// report a synthesized row built from the changesets row.
    pub async fn stat(&self, id: &ObjectId) -> Result<Option<ObjectRow>> {
        if id.kind() == Kind::Changeset {
            return Ok(self
                .changeset_row(id)
                .await?
                .map(|r| changeset_object_row(&r)));
        }
        let meta = &self.storage.meta;
        if let Some(Some(r)) = meta.get_objects(&[*id], Consistency::Stale).await?.pop() {
            return Ok(Some(r));
        }
        Ok(meta
            .get_objects(&[*id], Consistency::Strong)
            .await?
            .pop()
            .flatten())
    }

    /// [`ObjectRepo::stat`] for many ids (one stale batch, one strong batch for the misses).
    pub async fn stat_many(&self, ids: &[ObjectId]) -> Result<Vec<Option<ObjectRow>>> {
        let meta = &self.storage.meta;
        let mut out: Vec<Option<ObjectRow>> = vec![None; ids.len()];
        let (cs_idx, obj_idx): (Vec<usize>, Vec<usize>) =
            (0..ids.len()).partition(|&i| ids[i].kind() == Kind::Changeset);
        for consistency in [Consistency::Stale, Consistency::Strong] {
            let want: Vec<usize> = obj_idx
                .iter()
                .copied()
                .filter(|&i| out[i].is_none())
                .collect();
            if !want.is_empty() {
                let q: Vec<ObjectId> = want.iter().map(|&i| ids[i]).collect();
                let rows = meta.get_objects(&q, consistency).await?;
                for (i, r) in want.into_iter().zip(rows) {
                    out[i] = r;
                }
            }
            let want: Vec<usize> = cs_idx
                .iter()
                .copied()
                .filter(|&i| out[i].is_none())
                .collect();
            if !want.is_empty() {
                let q: Vec<ObjectId> = want.iter().map(|&i| ids[i]).collect();
                let rows = meta.get_changesets(&q, consistency).await?;
                for (i, r) in want.into_iter().zip(rows) {
                    out[i] = r.as_ref().map(changeset_object_row);
                }
            }
        }
        Ok(out)
    }

    /// Load and verify an object without counting the read.
    async fn load_verified(&self, id: &ObjectId) -> Result<(Bytes, StoredFrame, ObjectRow)> {
        if id.kind() == Kind::Changeset {
            let row = self.changeset_row(id).await?.ok_or_else(|| not_found(id))?;
            if !digest_matches(id, &row.encoded) {
                return Err(self.integrity_alert(id));
            }
            let obj = changeset_object_row(&row);
            return Ok((
                row.encoded.clone(),
                StoredFrame::Raw(row.encoded.clone()),
                obj,
            ));
        }
        let row = self.stat(id).await?.ok_or_else(|| not_found(id))?;
        if row.state != ObjectState::Live {
            return Err(ScionError::redacted(id));
        }
        let (raw, frame) = match &row.inline_data {
            Some(inline) => (inline.clone(), StoredFrame::Raw(inline.clone())),
            None => {
                let Some(frame) = self.storage.blobs.get(&self.key(id, Area::Live)).await? else {
                    // The bytes may have just moved to quarantine; re-check strongly.
                    let strong = self
                        .storage
                        .meta
                        .get_objects(&[*id], Consistency::Strong)
                        .await?;
                    if let Some(Some(r)) = strong.first()
                        && r.state != ObjectState::Live
                    {
                        return Err(ScionError::redacted(id));
                    }
                    return Err(self.integrity_alert(id));
                };
                let Some(raw) = decompress(frame.clone(), row.size).await else {
                    return Err(self.integrity_alert(id));
                };
                (raw, StoredFrame::Zstd(frame))
            }
        };
        if !verify_digest(id, &raw).await? {
            return Err(self.integrity_alert(id));
        }
        Ok((raw, frame, row))
    }

    /// Raw verified bytes: NOT_FOUND | REDACTED | INTEGRITY (+ alert).
    pub async fn get(&self, id: &ObjectId) -> Result<Bytes> {
        let (raw, _, _) = self.load_verified(id).await?;
        self.storage
            .counters
            .record_object_read(id.kind(), raw.len() as u64);
        Ok(raw)
    }

    /// Verified, then returned exactly as stored.
    pub async fn get_frame(&self, id: &ObjectId) -> Result<(StoredFrame, ObjectRow)> {
        let (raw, frame, row) = self.load_verified(id).await?;
        self.storage
            .counters
            .record_object_read(id.kind(), raw.len() as u64);
        Ok((frame, row))
    }

    /// Decoded tree through the LRU (immutable; survives a metadata outage).
    pub async fn get_tree(&self, id: &ObjectId) -> Result<Arc<Tree>> {
        if id.kind() != Kind::Tree {
            return Err(invalid_kind(format!("{id} is not a tree id")));
        }
        let hit = self.caches.trees.lock().get(id).cloned();
        if let Some((t, n)) = hit {
            self.storage.counters.record_object_read(Kind::Tree, n);
            return Ok(t);
        }
        let (raw, _, _) = self.load_verified(id).await?;
        let tree = Arc::new(Tree::decode(&raw)?);
        let n = raw.len() as u64;
        self.caches.trees.lock().put(*id, (tree.clone(), n));
        self.storage.counters.record_object_read(Kind::Tree, n);
        Ok(tree)
    }

    /// Decoded Meta through the LRU. A cached Meta is re-checked against its row state (Metas
    /// are redactable, SPEC 13.1); when the metadata store is unavailable the cached value is
    /// served (SPEC 16.1 class 1).
    pub async fn get_meta(&self, id: &ObjectId) -> Result<Arc<Meta>> {
        if id.kind() != Kind::Meta {
            return Err(invalid_kind(format!("{id} is not a meta id")));
        }
        let hit = self.caches.metas.lock().get(id).cloned();
        if let Some((m, n)) = hit {
            match self.stat(id).await {
                Ok(Some(row)) if row.state == ObjectState::Live => {}
                Ok(Some(_)) => {
                    self.caches.metas.lock().pop(id);
                    return Err(ScionError::redacted(id));
                }
                Ok(None) => {
                    self.caches.metas.lock().pop(id);
                    return Err(not_found(id));
                }
                Err(e) if e.code == ErrorCode::Unavailable => {}
                Err(e) => return Err(e),
            }
            self.storage.counters.record_object_read(Kind::Meta, n);
            return Ok(m);
        }
        let (raw, _, _) = self.load_verified(id).await?;
        let meta = Arc::new(Meta::decode(&raw)?);
        let n = raw.len() as u64;
        self.caches.metas.lock().put(*id, (meta.clone(), n));
        self.storage.counters.record_object_read(Kind::Meta, n);
        Ok(meta)
    }

    /// Decoded changeset and its row through the LRU.
    pub async fn get_changeset(&self, id: &ObjectId) -> Result<(Arc<Changeset>, ChangesetRow)> {
        if id.kind() != Kind::Changeset {
            return Err(invalid_kind(format!("{id} is not a changeset id")));
        }
        let hit = self.caches.changesets.lock().get(id).cloned();
        if let Some((c, row)) = hit {
            self.storage
                .counters
                .record_object_read(Kind::Changeset, row.encoded.len() as u64);
            return Ok((c, row));
        }
        let row = self.changeset_row(id).await?.ok_or_else(|| not_found(id))?;
        if !digest_matches(id, &row.encoded) {
            return Err(self.integrity_alert(id));
        }
        let cs = Arc::new(Changeset::decode(&row.encoded)?);
        self.caches
            .changesets
            .lock()
            .put(*id, (cs.clone(), row.clone()));
        self.storage
            .counters
            .record_object_read(Kind::Changeset, row.encoded.len() as u64);
        Ok((cs, row))
    }

    /// Copy `from` to `to`, then delete `from`; a missing source means there is nothing (left)
    /// to move (inline object or a completed earlier run). Both steps are idempotent.
    async fn move_bytes(&self, from: &str, to: &str, checkpoint: Option<&str>) -> Result<()> {
        match self.storage.blobs.copy(from, to).await {
            Ok(()) => {}
            Err(e) if e.code == ErrorCode::NotFound => {}
            Err(e) => return Err(e),
        }
        if let Some(p) = checkpoint {
            self.storage.faults.check(p).await?;
        }
        self.storage.blobs.delete(from).await
    }

    /// Copy live -> quarantine, then delete live (checkpoint `quarantine.after_copy`;
    /// SPEC 13.2).
    pub async fn quarantine_bytes(&self, id: &ObjectId) -> Result<()> {
        self.evict_cached(id);
        let live = self.key(id, Area::Live);
        let q = self.key(id, Area::Quarantine);
        self.move_bytes(&live, &q, Some(points::QUARANTINE_AFTER_COPY))
            .await
    }

    /// Copy quarantine -> live, then delete quarantine (SPEC 13.2 Unredact).
    pub async fn restore_bytes(&self, id: &ObjectId) -> Result<()> {
        let live = self.key(id, Area::Live);
        let q = self.key(id, Area::Quarantine);
        self.move_bytes(&q, &live, None).await
    }

    /// Delete both the live and quarantine keys (SPEC 13.2 Purge).
    pub async fn purge_bytes(&self, id: &ObjectId) -> Result<()> {
        self.evict_cached(id);
        self.storage.blobs.delete(&self.key(id, Area::Live)).await?;
        self.storage
            .blobs
            .delete(&self.key(id, Area::Quarantine))
            .await
    }
}
