//! The shared backend conformance suites on the in-memory backends (PLAN 3.2), both raw and
//! wrapped in the instrumented decorators (which must be transparent when no fault is armed).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test crate

use scion_core::FaultRegistry;
use scion_store::conformance::{blob_store_suite, meta_store_suite};
use scion_store::{
    BlobStore, Counters, InstrumentedBlob, InstrumentedMeta, MemBlobStore, MemMetaStore,
    MemOptions, MetaStore,
};
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn meta_store_suite_on_mem() {
    meta_store_suite(|| async {
        Arc::new(MemMetaStore::new(MemOptions::default())) as Arc<dyn MetaStore>
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn meta_store_suite_on_instrumented_mem() {
    meta_store_suite(|| async {
        Arc::new(InstrumentedMeta::new(
            MemMetaStore::new(MemOptions::default()),
            Arc::new(Counters::default()),
            FaultRegistry::new(),
        )) as Arc<dyn MetaStore>
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn meta_store_suite_with_stale_lag() {
    // Stale reads may lag; the suite never depends on a stale read observing a write.
    meta_store_suite(|| async {
        Arc::new(MemMetaStore::new(MemOptions {
            stale_lag_commits: 3,
            ..MemOptions::default()
        })) as Arc<dyn MetaStore>
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_store_suite_on_mem() {
    blob_store_suite(|| async { Arc::new(MemBlobStore::new()) as Arc<dyn BlobStore> }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_store_suite_on_instrumented_mem() {
    blob_store_suite(|| async {
        Arc::new(InstrumentedBlob::new(
            MemBlobStore::new(),
            Arc::new(Counters::default()),
            FaultRegistry::new(),
        )) as Arc<dyn BlobStore>
    })
    .await;
}
