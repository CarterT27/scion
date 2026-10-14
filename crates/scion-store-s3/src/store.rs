//! The S3 object store (SPEC 5.1, 3.5; `docs/contracts/storage-semantics.md` section 6).
//!
//! `object_store`'s `AmazonS3` client behind [`BlobStore`]: endpoint override, path-style
//! addressing and plain HTTP for MinIO. Every key is stored under `S3Config.key_prefix`.
//! `get` of a missing key is `Ok(None)`, `delete` is idempotent, `copy` of a missing source is
//! NOT_FOUND, and `list(prefix)` is a plain string-prefix listing returned in ascending order.
//! Backend failures are UNAVAILABLE/store_unavailable (SPEC 16.1 class 2).

use async_trait::async_trait;
use bytes::Bytes;
use futures::TryStreamExt;
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path;
use object_store::{ClientOptions, ObjectStore, PutPayload, RetryConfig};
use scion_core::{ErrorCode, Result, ScionError, subcode};
use scion_store::BlobStore;
use std::time::Duration;

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

/// `object_store` AmazonS3 behind [`BlobStore`] (SPEC 5.1; PLAN 2.4).
#[derive(Debug)]
pub struct S3BlobStore {
    store: AmazonS3,
    /// `key_prefix` without surrounding `/` (empty: keys are used as given).
    prefix: String,
}

fn unavailable(op: &str, key: &str, e: impl std::fmt::Display) -> ScionError {
    tracing::warn!(target: "scion::store::s3", op, key, error = %e, "object storage error");
    ScionError::unavailable(
        subcode::STORE_UNAVAILABLE,
        format!("object storage {op} {key}: {e}"),
    )
}

impl S3BlobStore {
    /// Build the client (no IO). An invalid configuration (e.g. a malformed endpoint) is
    /// INVALID_ARGUMENT.
    pub fn new(cfg: S3Config) -> Result<Self> {
        let mut b = AmazonS3Builder::new()
            .with_endpoint(cfg.endpoint.clone())
            .with_bucket_name(cfg.bucket.clone())
            .with_region(cfg.region.clone())
            .with_allow_http(cfg.allow_http)
            .with_virtual_hosted_style_request(!cfg.path_style)
            .with_retry(RetryConfig {
                max_retries: 3,
                retry_timeout: Duration::from_secs(20),
                ..RetryConfig::default()
            })
            .with_client_options(
                ClientOptions::new()
                    .with_allow_http(cfg.allow_http)
                    .with_connect_timeout(Duration::from_secs(5))
                    .with_timeout(Duration::from_secs(60)),
            );
        if let Some(k) = &cfg.access_key {
            b = b.with_access_key_id(k.clone());
        }
        if let Some(s) = &cfg.secret_key {
            b = b.with_secret_access_key(s.clone());
        }
        if cfg.access_key.is_none() && cfg.secret_key.is_none() {
            // No static credentials: anonymous requests (public or test buckets).
            b = b.with_skip_signature(true);
        }
        let store = b.build().map_err(|e| {
            ScionError::new(
                ErrorCode::InvalidArgument,
                format!("invalid object storage configuration: {e}"),
            )
        })?;
        Ok(S3BlobStore {
            store,
            prefix: cfg.key_prefix.trim_matches('/').to_owned(),
        })
    }

    /// The full object key of `key` (with the configured prefix).
    fn full_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{}/{key}", self.prefix)
        }
    }

    /// The `object_store` path of `key`; keys that are not valid S3 paths (empty segments,
    /// `.`/`..` segments, control characters) are INVALID_ARGUMENT.
    fn path(&self, key: &str) -> Result<Path> {
        let full = self.full_key(key);
        Path::parse(&full).map_err(|e| {
            ScionError::new(
                ErrorCode::InvalidArgument,
                format!("invalid object key {key:?}: {e}"),
            )
        })
    }

    /// Strip the configured prefix from a listed location.
    fn strip<'a>(&self, location: &'a str) -> Option<&'a str> {
        if self.prefix.is_empty() {
            Some(location)
        } else {
            location
                .strip_prefix(self.prefix.as_str())
                .and_then(|r| r.strip_prefix('/'))
        }
    }
}

#[async_trait]
impl BlobStore for S3BlobStore {
    async fn put(&self, key: &str, bytes: Bytes) -> Result<()> {
        let path = self.path(key)?;
        self.store
            .put(&path, PutPayload::from(bytes))
            .await
            .map_err(|e| unavailable("put", key, e))?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        let path = self.path(key)?;
        match self.store.get(&path).await {
            Ok(r) => r
                .bytes()
                .await
                .map(Some)
                .map_err(|e| unavailable("get", key, e)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(unavailable("get", key, e)),
        }
    }

    async fn copy(&self, from: &str, to: &str) -> Result<()> {
        let (src, dst) = (self.path(from)?, self.path(to)?);
        match self.store.copy(&src, &dst).await {
            Ok(()) => Ok(()),
            Err(object_store::Error::NotFound { .. }) => Err(ScionError::not_found(format!(
                "object storage copy: source {from} not found"
            ))),
            Err(e) => {
                // Some S3 implementations report a missing copy source with a non-404 status;
                // distinguish it with a point check.
                if !self.exists(from).await? {
                    return Err(ScionError::not_found(format!(
                        "object storage copy: source {from} not found"
                    )));
                }
                Err(unavailable("copy", from, e))
            }
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.path(key)?;
        match self.store.delete(&path).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(unavailable("delete", key, e)),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let path = self.path(key)?;
        match self.store.head(&path).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(unavailable("exists", key, e)),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        // object_store lists by whole path segments, so list the deepest complete directory
        // of `prefix` and filter by the string prefix.
        let dir = match prefix.rfind('/') {
            Some(i) => &prefix[..i],
            None => "",
        };
        let full_dir = match (self.prefix.is_empty(), dir.is_empty()) {
            (true, _) => dir.to_owned(),
            (false, true) => self.prefix.clone(),
            (false, false) => format!("{}/{dir}", self.prefix),
        };
        let listing = if full_dir.is_empty() {
            None
        } else {
            Some(Path::parse(&full_dir).map_err(|e| {
                ScionError::new(
                    ErrorCode::InvalidArgument,
                    format!("invalid object key prefix {prefix:?}: {e}"),
                )
            })?)
        };
        let metas: Vec<object_store::ObjectMeta> = self
            .store
            .list(listing.as_ref())
            .try_collect()
            .await
            .map_err(|e| unavailable("list", prefix, e))?;
        let mut keys: Vec<String> = metas
            .iter()
            .filter_map(|m| self.strip(m.location.as_ref()))
            .filter(|k| k.starts_with(prefix))
            .map(str::to_owned)
            .collect();
        keys.sort();
        keys.dedup();
        Ok(keys)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(prefix: &str) -> S3Config {
        S3Config {
            endpoint: "http://127.0.0.1:9".to_owned(),
            bucket: "b".to_owned(),
            region: "us-east-1".to_owned(),
            access_key: Some("k".to_owned()),
            secret_key: Some("s".to_owned()),
            allow_http: true,
            path_style: true,
            key_prefix: prefix.to_owned(),
        }
    }

    #[test]
    fn keys_carry_the_prefix() {
        let s = S3BlobStore::new(cfg("/run-1/")).unwrap();
        assert_eq!(s.full_key("scion/r/1/b/aa/x"), "run-1/scion/r/1/b/aa/x");
        assert_eq!(s.strip("run-1/scion/r/1"), Some("scion/r/1"));
        assert_eq!(s.strip("run-10/scion"), None);
        assert_eq!(s.strip("other/x"), None);
        let bare = S3BlobStore::new(cfg("")).unwrap();
        assert_eq!(bare.full_key("a/b"), "a/b");
        assert_eq!(bare.strip("a/b"), Some("a/b"));
    }

    #[test]
    fn invalid_keys_are_invalid_argument() {
        let s = S3BlobStore::new(cfg("p")).unwrap();
        assert!(s.path("a/b/c").is_ok());
        assert_eq!(s.path("a//b").unwrap_err().code, ErrorCode::InvalidArgument);
        assert_eq!(
            s.path("a/../b").unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }
}
