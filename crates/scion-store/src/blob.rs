//! The object storage trait (SPEC 5.1).

use async_trait::async_trait;
use bytes::Bytes;
use scion_core::Result;
use std::sync::Arc;

/// S3-compatible object storage with strong read-after-write consistency (SPEC 3.5).
#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    /// Store `bytes` at `key` (overwrite allowed; content-addressed keys make it idempotent).
    async fn put(&self, key: &str, bytes: Bytes) -> Result<()>;
    /// The bytes at `key`, or `None` when absent.
    async fn get(&self, key: &str) -> Result<Option<Bytes>>;
    /// Copy `from` to `to` (SPEC 13.2 quarantine moves).
    async fn copy(&self, from: &str, to: &str) -> Result<()>;
    /// Delete `key`; idempotent.
    async fn delete(&self, key: &str) -> Result<()>;
    /// True when `key` exists.
    async fn exists(&self, key: &str) -> Result<bool>;
    /// Keys with the given prefix, ascending.
    async fn list(&self, prefix: &str) -> Result<Vec<String>>;
}

/// Forwarding impl so decorators can wrap shared handles.
#[async_trait]
impl<T: BlobStore + ?Sized> BlobStore for Arc<T> {
    async fn put(&self, key: &str, bytes: Bytes) -> Result<()> {
        (**self).put(key, bytes).await
    }
    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        (**self).get(key).await
    }
    async fn copy(&self, from: &str, to: &str) -> Result<()> {
        (**self).copy(from, to).await
    }
    async fn delete(&self, key: &str) -> Result<()> {
        (**self).delete(key).await
    }
    async fn exists(&self, key: &str) -> Result<bool> {
        (**self).exists(key).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        (**self).list(prefix).await
    }
}
