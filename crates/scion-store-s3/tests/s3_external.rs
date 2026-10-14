//! S3/MinIO backend tests (PLAN 3.3; SPEC 3.5, 5.1, 16.1 class 2, 19.3 on a real backend).
//!
//! Every `external_*` test is gated on `SCION_TEST_S3_ENDPOINT`: when it is unset the test
//! prints `SKIP: ...` and passes. Each store gets a unique key prefix (`test_config`). The
//! remaining tests are hermetic.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test crate

use bytes::Bytes;
use scion_core::{ErrorCode, FaultAction, FaultRegistry, ObjectId, subcode};
use scion_store::{
    Area, BlobStore, Consistency, InsertOutcome, MemMetaStore, MemOptions, MetaStore, ObjectRepo,
    ObjectRepoConfig, ObjectState, Storage, conformance,
};
use scion_store_s3::{S3BlobStore, S3Config, test_config};
use std::sync::Arc;

/// The gate: `Some(config)` (with a fresh key prefix) when the external S3 is configured.
fn gate(test: &str) -> Option<S3Config> {
    let cfg = test_config();
    if cfg.is_none() {
        println!("SKIP: SCION_TEST_S3_ENDPOINT unset ({test})");
    }
    cfg
}

fn store(cfg: S3Config) -> Arc<S3BlobStore> {
    Arc::new(S3BlobStore::new(cfg).expect("S3 client"))
}

// ---------------------------------------------------------------------------------------------
// Hermetic tests (no object storage needed)
// ---------------------------------------------------------------------------------------------

fn unreachable_cfg() -> S3Config {
    S3Config {
        // Nothing listens on port 1 of the loopback interface.
        endpoint: "http://127.0.0.1:1".to_owned(),
        bucket: "scion-test".to_owned(),
        region: "us-east-1".to_owned(),
        access_key: Some("minioadmin".to_owned()),
        secret_key: Some("minioadmin".to_owned()),
        allow_http: true,
        path_style: true,
        key_prefix: "unreachable".to_owned(),
    }
}

#[tokio::test]
async fn s3_unreachable_endpoint_is_store_unavailable() {
    // SPEC 16.1 class 2: object storage down -> UNAVAILABLE.
    let s = S3BlobStore::new(unreachable_cfg()).unwrap();
    for e in [
        s.put("k/x", Bytes::from_static(b"x")).await.unwrap_err(),
        s.get("k/x").await.unwrap_err(),
        s.exists("k/x").await.unwrap_err(),
        s.list("k/").await.unwrap_err(),
    ] {
        assert_eq!(e.code, ErrorCode::Unavailable, "{e}");
        assert_eq!(e.subcode(), Some(subcode::STORE_UNAVAILABLE), "{e}");
    }
}

#[test]
fn s3_new_does_no_io_and_rejects_bad_configuration() {
    assert!(S3BlobStore::new(unreachable_cfg()).is_ok());
    let mut bad = unreachable_cfg();
    bad.bucket = String::new();
    let e = S3BlobStore::new(bad).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e}");
}

#[test]
fn test_config_prefixes_are_unique() {
    if let (Some(a), Some(b)) = (test_config(), test_config()) {
        assert_ne!(a.key_prefix, b.key_prefix);
        assert!(a.path_style);
    } else {
        println!("SKIP: SCION_TEST_S3_ENDPOINT unset (test_config_prefixes_are_unique)");
    }
}

// ---------------------------------------------------------------------------------------------
// External tests
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_s3_blob_store_conformance() {
    if gate("external_s3_blob_store_conformance").is_none() {
        return;
    }
    conformance::blob_store_suite(|| async {
        let cfg = test_config().expect("gate checked");
        store(cfg) as Arc<dyn BlobStore>
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_s3_key_prefixes_isolate_stores() {
    let Some(cfg_a) = gate("external_s3_key_prefixes_isolate_stores") else {
        return;
    };
    let cfg_b = test_config().unwrap();
    let (a, b) = (store(cfg_a.clone()), store(cfg_b));
    a.put("scion/r/1/b/aa/k", Bytes::from_static(b"a"))
        .await
        .unwrap();
    assert!(a.exists("scion/r/1/b/aa/k").await.unwrap());
    assert!(!b.exists("scion/r/1/b/aa/k").await.unwrap());
    assert_eq!(b.get("scion/r/1/b/aa/k").await.unwrap(), None);
    assert!(b.list("scion/").await.unwrap().is_empty());
    assert_eq!(
        a.list("scion/").await.unwrap(),
        vec!["scion/r/1/b/aa/k".to_owned()],
        "listed keys are returned without the store prefix"
    );
    // A store with the same prefix sees the same keys (strong read-after-write, SPEC 3.5).
    let a2 = store(cfg_a);
    assert_eq!(
        a2.get("scion/r/1/b/aa/k").await.unwrap(),
        Some(Bytes::from_static(b"a"))
    );
    a2.delete("scion/r/1/b/aa/k").await.unwrap();
    assert!(!a.exists("scion/r/1/b/aa/k").await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_s3_list_paginates() {
    let Some(cfg) = gate("external_s3_list_paginates") else {
        return;
    };
    let s = store(cfg);
    // More keys than one ListObjectsV2 page (1000).
    let n = 1100usize;
    let mut handles = Vec::new();
    for chunk in 0..11usize {
        let s = s.clone();
        handles.push(tokio::spawn(async move {
            for i in chunk * 100..(chunk + 1) * 100 {
                s.put(&format!("page/{i:05}"), Bytes::from_static(b"x"))
                    .await
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let keys = s.list("page/").await.unwrap();
    assert_eq!(keys.len(), n);
    assert!(keys.windows(2).all(|w| w[0] < w[1]), "ascending");
    assert_eq!(s.list("page/001").await.unwrap().len(), 100);
    assert_eq!(s.list("page/0001").await.unwrap().len(), 10);
}

fn repo_over_s3(blobs: Arc<S3BlobStore>) -> (ObjectRepo, Arc<MemMetaStore>) {
    let meta = Arc::new(MemMetaStore::new(MemOptions::default()));
    let storage = Storage::new(meta.clone(), blobs, FaultRegistry::new());
    (ObjectRepo::new(storage, ObjectRepoConfig::default()), meta)
}

/// SPEC 19.3 on S3: a put that fails leaves no `objects` row (and no bytes).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_s3_object_repo_put_failure_leaves_no_row() {
    let Some(cfg) = gate("external_s3_object_repo_put_failure_leaves_no_row") else {
        return;
    };
    let blobs = store(cfg);
    let (repo, meta) = repo_over_s3(blobs.clone());
    repo.storage().faults.arm(
        "blob.put.before",
        FaultAction::Error(ErrorCode::Unavailable),
        Some(1),
    );
    let content = Bytes::from_static(b"never durable on s3");
    let id = ObjectId::hash_blob(&content);
    let e = repo.put_blob(content.clone()).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unavailable);
    assert_eq!(
        meta.table_counts().objects,
        0,
        "no row without durable bytes"
    );
    assert!(!blobs.exists(&repo.key(&id, Area::Live)).await.unwrap());
    assert!(blobs.list("").await.unwrap().is_empty());
    assert_eq!(repo.stat(&id).await.unwrap(), None);
    // Once the fault is gone the upload completes and reads back verified bytes.
    let (got, outcome) = repo.put_blob(content.clone()).await.unwrap();
    assert_eq!((got, outcome), (id, InsertOutcome::Inserted));
    assert_eq!(repo.get(&id).await.unwrap(), content);
}

/// SPEC 19.3 on S3: FailAfter on `meta.insert_object` leaves a durable row and durable bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_s3_object_repo_fail_after_insert_leaves_row_and_bytes() {
    let Some(cfg) = gate("external_s3_object_repo_fail_after_insert_leaves_row_and_bytes") else {
        return;
    };
    let blobs = store(cfg);
    let (repo, meta) = repo_over_s3(blobs.clone());
    repo.storage()
        .faults
        .arm("meta.insert_object.after", FaultAction::FailAfter, Some(1));
    let content = Bytes::from(vec![42u8; 200_000]);
    let id = ObjectId::hash_blob(&content);
    let e = repo.put_blob(content.clone()).await.unwrap_err();
    assert_eq!(e.subcode(), Some(subcode::INJECTED));
    let row = meta.get_objects(&[id], Consistency::Strong).await.unwrap()[0]
        .clone()
        .expect("row committed");
    assert_eq!(row.state, ObjectState::Live);
    let stored = blobs
        .get(&repo.key(&id, Area::Live))
        .await
        .unwrap()
        .expect("durable bytes behind the row");
    assert_eq!(stored.len() as u64, row.stored_size);
    assert_eq!(repo.get(&id).await.unwrap(), content);
    let (_, outcome) = repo.put_blob(content.clone()).await.unwrap();
    assert_eq!(outcome, InsertOutcome::AlreadyExists);

    // Quarantine moves on S3 are idempotent copy-then-delete (SPEC 13.2).
    repo.quarantine_bytes(&id).await.unwrap();
    repo.quarantine_bytes(&id).await.unwrap();
    assert!(!blobs.exists(&repo.key(&id, Area::Live)).await.unwrap());
    assert!(
        blobs
            .exists(&repo.key(&id, Area::Quarantine))
            .await
            .unwrap()
    );
    repo.restore_bytes(&id).await.unwrap();
    assert!(blobs.exists(&repo.key(&id, Area::Live)).await.unwrap());
    repo.purge_bytes(&id).await.unwrap();
    assert!(blobs.list("").await.unwrap().is_empty());
}
