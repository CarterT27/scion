//! The storage bundle (PLAN 1.4, 2.3).

use crate::blob::BlobStore;
use crate::instrument::{Counters, InstrumentedBlob, InstrumentedMeta};
use crate::mem_blob::MemBlobStore;
use crate::mem_meta::{MemMetaStore, MemOptions};
use crate::meta::MetaStore;
use scion_core::FaultRegistry;
use std::sync::Arc;

/// Both backends, wrapped in the counting/fault decorators (SPEC 19.0), plus the shared
/// counters and fault registry.
#[derive(Clone)]
pub struct Storage {
    /// Instrumented metadata store.
    pub meta: Arc<dyn MetaStore>,
    /// Instrumented object storage.
    pub blobs: Arc<dyn BlobStore>,
    /// Storage access counters (SPEC 19.0).
    pub counters: Arc<Counters>,
    /// Fault registry shared by the decorators and checkpoints (SPEC 19.0).
    pub faults: Arc<FaultRegistry>,
}

impl Storage {
    /// Wrap `meta` and `blobs` in [`InstrumentedMeta`] / [`InstrumentedBlob`] with fresh
    /// counters and the given fault registry.
    pub fn new(
        meta: Arc<dyn MetaStore>,
        blobs: Arc<dyn BlobStore>,
        faults: Arc<FaultRegistry>,
    ) -> Self {
        let counters = Arc::new(Counters::default());
        let meta: Arc<dyn MetaStore> = Arc::new(InstrumentedMeta::new(
            meta,
            counters.clone(),
            faults.clone(),
        ));
        let blobs: Arc<dyn BlobStore> = Arc::new(InstrumentedBlob::new(
            blobs,
            counters.clone(),
            faults.clone(),
        ));
        Storage {
            meta,
            blobs,
            counters,
            faults,
        }
    }

    /// Hermetic in-memory storage plus direct handles to the raw backends.
    pub fn in_memory(opts: MemOptions) -> (Self, MemHandles) {
        let meta = Arc::new(MemMetaStore::new(opts));
        let blobs = Arc::new(MemBlobStore::new());
        let storage = Storage::new(meta.clone(), blobs.clone(), FaultRegistry::new());
        (storage, MemHandles { meta, blobs })
    }
}

/// Direct (undecorated) handles to the in-memory backends, for test introspection.
#[derive(Clone)]
pub struct MemHandles {
    /// The in-memory metadata store.
    pub meta: Arc<MemMetaStore>,
    /// The in-memory object storage.
    pub blobs: Arc<MemBlobStore>,
}
