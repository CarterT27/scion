//! The S3 object store (P15).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use async_trait::async_trait;
use bytes::Bytes;
use scion_core::Result;
use scion_store::BlobStore;

/// S3/MinIO settings (SPEC 6.1 `[objects]`, PLAN 6.11).
#[derive(Clone, Debug)]
pub struct S3Config {
    /// Endpoint URL.
    pub endpoint: String,
    /// Bucket.
    pub bucket: String,
    /// Region.
    pub region: String,
    /// Access key.
    pub access_key: Option<String>,
    /// Secret key.
    pub secret_key: Option<String>,
    /// Allow plain HTTP (MinIO in tests).
    pub allow_http: bool,
    /// Path-style addressing.
    pub path_style: bool,
    /// Prefix prepended to every key.
    pub key_prefix: String,
}

/// `object_store` AmazonS3 behind [`BlobStore`].
pub struct S3BlobStore {
    cfg: S3Config,
}

impl S3BlobStore {
    /// Build the client (no IO).
    pub fn new(cfg: S3Config) -> Result<Self> {
        todo!("P15")
    }
}

#[async_trait]
impl BlobStore for S3BlobStore {
    async fn put(&self, key: &str, bytes: Bytes) -> Result<()> {
        todo!("P15")
    }
    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        todo!("P15")
    }
    async fn copy(&self, from: &str, to: &str) -> Result<()> {
        todo!("P15")
    }
    async fn delete(&self, key: &str) -> Result<()> {
        todo!("P15")
    }
    async fn exists(&self, key: &str) -> Result<bool> {
        todo!("P15")
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        todo!("P15")
    }
}

/// Gated test configuration: `SCION_TEST_S3_ENDPOINT` (gate), `_BUCKET` (default
/// `scion-test`), `_ACCESS_KEY`, `_SECRET_KEY`, `_REGION` (default `us-east-1`), and a unique
/// key prefix per call (PLAN 3.3).
pub fn test_config() -> Option<S3Config> {
    let var = |n: &str| std::env::var(n).ok().filter(|s| !s.trim().is_empty());
    let endpoint = var("SCION_TEST_S3_ENDPOINT")?;
    Some(S3Config {
        allow_http: endpoint.starts_with("http://"),
        bucket: var("SCION_TEST_S3_BUCKET").unwrap_or_else(|| "scion-test".to_owned()),
        region: var("SCION_TEST_S3_REGION").unwrap_or_else(|| "us-east-1".to_owned()),
        access_key: var("SCION_TEST_S3_ACCESS_KEY"),
        secret_key: var("SCION_TEST_S3_SECRET_KEY"),
        path_style: true,
        key_prefix: format!("scion-test-{}", uuid::Uuid::new_v4().simple()),
        endpoint,
    })
}
