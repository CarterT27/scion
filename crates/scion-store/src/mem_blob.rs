//! Hermetic in-memory [`BlobStore`] (PLAN 3.2).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::blob::BlobStore;
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use scion_core::Result;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

/// In-memory object storage with corruption and outage switches.
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
        todo!("P03")
    }

    /// The raw stored bytes at `key`.
    pub fn raw(&self, key: &str) -> Option<Bytes> {
        todo!("P03")
    }

    /// Flip one bit of the value at `key` (SPEC 16.1 class 7 tests).
    pub fn corrupt(&self, key: &str) {
        todo!("P03")
    }

    /// When down, every operation fails UNAVAILABLE/store_unavailable.
    pub fn set_down(&self, down: bool) {
        todo!("P03")
    }
}

#[async_trait]
impl BlobStore for MemBlobStore {
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
