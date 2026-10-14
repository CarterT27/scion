//! Shared backend conformance suites (always compiled; test-support API, PLAN 3.2).
//!
//! The in-memory backends always run them; CockroachDB/S3 run them when env-gated (P15).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::blob::BlobStore;
use crate::meta::MetaStore;
use std::future::Future;
use std::sync::Arc;

/// Every [`MetaStore`] method contract, including concurrency cases. `factory` returns a fresh,
/// empty store per case.
pub async fn meta_store_suite<F, Fut>(factory: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn MetaStore>>,
{
    todo!("P03")
}

/// Every [`BlobStore`] method contract. `factory` returns a fresh, empty store per case.
pub async fn blob_store_suite<F, Fut>(factory: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn BlobStore>>,
{
    todo!("P03")
}
