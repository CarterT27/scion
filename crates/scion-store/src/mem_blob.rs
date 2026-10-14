//! Hermetic in-memory [`BlobStore`] (PLAN 3.2, `docs/contracts/storage-semantics.md` 5-6).

use crate::blob::BlobStore;
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use scion_core::{Result, ScionError, subcode};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::atomic::{AtomicBool, Ordering};

/// In-memory object storage with corruption and outage switches (SPEC 5.1, 16.1 classes 2/7).
#[derive(Debug, Default)]
pub struct MemBlobStore {
    objects: Mutex<BTreeMap<String, Bytes>>,
    down: AtomicBool,
}

impl MemBlobStore {
    /// An empty store.
    pub fn new() -> Self {
        MemBlobStore::default()
    }

    /// Every key, ascending.
    pub fn keys(&self) -> Vec<String> {
        self.objects.lock().keys().cloned().collect()
    }

    /// The raw stored bytes at `key`.
    pub fn raw(&self, key: &str) -> Option<Bytes> {
        self.objects.lock().get(key).cloned()
    }

    /// Flip one bit of the value at `key` (SPEC 16.1 class 7 tests). An empty value becomes one
    /// byte; a missing key is left alone.
    pub fn corrupt(&self, key: &str) {
        let mut objects = self.objects.lock();
        if let Some(v) = objects.get_mut(key) {
            let mut b = v.to_vec();
            match b.last_mut() {
                Some(last) => *last ^= 0x01,
                None => b.push(0x01),
            }
            *v = Bytes::from(b);
        }
    }

    /// When down, every operation fails UNAVAILABLE/store_unavailable (SPEC 16.1 class 2).
    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    /// Overwrite the value at `key` directly, bypassing the outage switch (SPEC 19.0 test
    /// support).
    pub fn insert_raw(&self, key: &str, bytes: Bytes) {
        self.objects.lock().insert(key.to_owned(), bytes);
    }

    /// Remove `key` directly, bypassing the outage switch (SPEC 19.0 test support).
    pub fn remove_raw(&self, key: &str) {
        self.objects.lock().remove(key);
    }

    fn up(&self) -> Result<()> {
        if self.down.load(Ordering::SeqCst) {
            return Err(ScionError::unavailable(
                subcode::STORE_UNAVAILABLE,
                "object storage is down",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl BlobStore for MemBlobStore {
    async fn put(&self, key: &str, bytes: Bytes) -> Result<()> {
        self.up()?;
        self.objects.lock().insert(key.to_owned(), bytes);
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        self.up()?;
        Ok(self.objects.lock().get(key).cloned())
    }

    async fn copy(&self, from: &str, to: &str) -> Result<()> {
        self.up()?;
        let mut objects = self.objects.lock();
        let v = objects
            .get(from)
            .cloned()
            .ok_or_else(|| ScionError::not_found(format!("copy source {from} does not exist")))?;
        objects.insert(to.to_owned(), v);
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.up()?;
        self.objects.lock().remove(key);
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        self.up()?;
        Ok(self.objects.lock().contains_key(key))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.up()?;
        let objects = self.objects.lock();
        Ok(objects
            .range::<str, _>((Bound::Included(prefix), Bound::Unbounded))
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, _)| k.clone())
            .collect())
    }
}
