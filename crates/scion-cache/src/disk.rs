//! Verified content-addressed disk cache (SPEC 10.2; P10).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use bytes::Bytes;
use scion_core::{ObjectId, Result};
use std::path::Path;

/// A payload as received from upstream.
#[derive(Clone, Debug)]
pub enum Payload {
    /// Raw content.
    Raw(Bytes),
    /// A zstd frame of the content.
    Zstd(Bytes),
}

/// `<dir>/objects/<kind_letter>/<hex[0:2]>/<hex>` with approximate-LRU eviction to the low
/// watermark.
pub struct DiskCache {
    _private: (),
}

impl DiskCache {
    /// Open (creating) the cache: deletes leftover temp files and rebuilds the index.
    pub fn open(
        dir: &Path,
        max_bytes: u64,
        low_watermark: f64,
        verify_on_read: bool,
    ) -> Result<Self> {
        todo!("P10")
    }

    /// Raw content, verified when `verify_on_read`; a mismatch deletes the entry and returns
    /// `Ok(None)`.
    pub fn get(&self, id: &ObjectId) -> Result<Option<Bytes>> {
        todo!("P10")
    }

    /// The payload as stored.
    pub fn get_payload(&self, id: &ObjectId) -> Result<Option<Payload>> {
        todo!("P10")
    }

    /// Insert: temp file in the same directory, verify the digest (INTEGRITY), fsync, rename;
    /// may evict.
    pub fn insert(&self, id: &ObjectId, p: Payload) -> Result<()> {
        todo!("P10")
    }

    /// Remove an entry (used on redaction).
    pub fn remove(&self, id: &ObjectId) -> Result<()> {
        todo!("P10")
    }

    /// True when cached.
    pub fn contains(&self, id: &ObjectId) -> bool {
        todo!("P10")
    }

    /// Exact bytes used.
    pub fn usage(&self) -> u64 {
        todo!("P10")
    }
}
