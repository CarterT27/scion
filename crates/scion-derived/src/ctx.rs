//! [`DeriveCtx`] over an [`ObjectRepo`] (SPEC 9.1, 13.5; P14).

use crate::types::DeriveCtx;
use async_trait::async_trait;
use bytes::Bytes;
use scion_core::{ErrorCode, Kind, ObjectId, Result};
use scion_history::ChangesetRecord;
use scion_object::{Meta, Tree};
use scion_store::{Consistency, DerivedKey, DerivedRow, ObjectRepo};
use std::sync::Arc;

/// The production derivation context (SPEC 9.1).
///
/// - [`DeriveCtx::blob_or_empty`] reads quarantined or purged content as empty (SPEC 13.5).
/// - [`DeriveCtx::meta`] returns `None` for a redacted or purged Meta (SPEC 13.4).
/// - [`DeriveCtx::put_object`] stores through `ObjectRepo::put_encoded` (SPEC 5.3 step 1).
#[derive(Clone)]
pub struct StoreDeriveCtx {
    objects: ObjectRepo,
}

impl StoreDeriveCtx {
    /// A context over `objects`.
    pub fn new(objects: ObjectRepo) -> Self {
        StoreDeriveCtx { objects }
    }

    /// The underlying object facade.
    pub fn objects(&self) -> &ObjectRepo {
        &self.objects
    }
}

/// The value of a stored derived row: inline `value`, else the raw bytes at `storage_key` in
/// object storage; a row whose out-of-line bytes are gone counts as absent (SPEC 5.2 `derived`).
pub(crate) async fn row_value(objects: &ObjectRepo, row: DerivedRow) -> Result<Option<Bytes>> {
    if let Some(v) = row.value {
        return Ok(Some(v));
    }
    match row.storage_key {
        Some(k) => objects.storage().blobs.get(&k).await,
        None => Ok(None),
    }
}

/// Read a stored derived value: stale read first, then a strong read on a miss (SPEC 5.4).
pub(crate) async fn read_stored(objects: &ObjectRepo, key: &DerivedKey) -> Result<Option<Bytes>> {
    let meta = &objects.storage().meta;
    for c in [Consistency::Stale, Consistency::Strong] {
        if let Some(row) = meta.get_derived(key, c).await?
            && let Some(v) = row_value(objects, row).await?
        {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

#[async_trait]
impl DeriveCtx for StoreDeriveCtx {
    async fn changeset(&self, id: &ObjectId) -> Result<ChangesetRecord> {
        let (changeset, row) = self.objects.get_changeset(id).await?;
        Ok(ChangesetRecord {
            id: *id,
            changeset,
            generation: row.generation,
            skip: row.skip,
            created_by: row.created_by,
        })
    }

    async fn tree(&self, id: &ObjectId) -> Result<Arc<Tree>> {
        self.objects.get_tree(id).await
    }

    async fn meta(&self, id: &ObjectId) -> Result<Option<Arc<Meta>>> {
        match self.objects.get_meta(id).await {
            Ok(m) => Ok(Some(m)),
            Err(e) if e.code == ErrorCode::Redacted => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn blob_or_empty(&self, id: &ObjectId) -> Result<Bytes> {
        match self.objects.get(id).await {
            Ok(b) => Ok(b),
            Err(e) if e.code == ErrorCode::Redacted => Ok(Bytes::new()),
            Err(e) => Err(e),
        }
    }

    async fn get_object(&self, id: &ObjectId) -> Result<Bytes> {
        self.objects.get(id).await
    }

    async fn put_object(&self, kind: Kind, canonical: Bytes) -> Result<ObjectId> {
        let (id, _) = self.objects.put_encoded(kind, canonical).await?;
        Ok(id)
    }

    async fn stored(&self, key: &DerivedKey) -> Result<Option<Bytes>> {
        read_stored(&self.objects, key).await
    }
}
