//! SPEC 19.3 storage-layer items at the RPC level (P33), on the Memory backend and, as
//! `external_*`, on CockroachDB + S3 when the gate variables are set:
//!
//! - An `objects` row never exists without durable bytes: object storage failing before or
//!   after the byte write (fault decorators, the in-memory outage switch) never leaves a row,
//!   and a sweep over every live row finds its bytes.
//! - Inline threshold: trees and Metas whose encoding is `threshold - 1`, `threshold` and
//!   `threshold + 1` bytes (built by CreateChangeset, or uploaded with UploadObjects first)
//!   are stored inline exactly up to the threshold and in object storage above it; blobs of
//!   those sizes always go to object storage; everything reads back byte-identically through
//!   GetObjects.
//! - A stale-read miss is retried strongly before NOT_FOUND (in-memory stale view frozen).
//! - Orphan bytes (crash leftovers and foreign bytes at an object's key) affect no API result.
//!
//! Every test ends with `invariants::check_16_2`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use scion_conformance::{Backend, ClusterOpts, TestCluster, Via, invariants};
use scion_core::{ErrorCode, FaultAction, Kind, Mode, ObjectId, Principal, fault::points};
use scion_object::{Canonical, Extensions, Name, Tree, TreeEntry};
use scion_proto::{ScionClient, error_from_status, v1};
use scion_store::{Area, Consistency, ObjectState};

/// SPEC 6.5 default `objects.inline_threshold_bytes`.
const THRESHOLD: usize = 65536;

fn gh(login: &str) -> Principal {
    TestCluster::gh(login)
}

fn skip_external() -> bool {
    if Backend::external_available() {
        return false;
    }
    println!("SKIP: SCION_TEST_CRDB_URL / SCION_TEST_S3_ENDPOINT unset");
    true
}

fn idb(id: &ObjectId) -> Vec<u8> {
    id.as_bytes().to_vec()
}

fn tag() -> String {
    scion_core::ChangeId::random().to_hex()
}

/// Deterministic incompressible bytes.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

async fn try_upload(
    client: &ScionClient,
    items: Vec<(ObjectId, Vec<u8>)>,
) -> Result<v1::UploadObjectsResponse, tonic::Status> {
    let items: Vec<v1::UploadItem> = items
        .into_iter()
        .map(|(id, data)| v1::UploadItem {
            id: idb(&id),
            kind: u32::from(id.kind().byte()),
            data,
        })
        .collect();
    client
        .write()
        .upload_objects(futures::stream::iter(items))
        .await
        .map(|r| r.into_inner())
}

async fn create(
    client: &ScionClient,
    req: v1::CreateChangesetRequest,
) -> Result<v1::CreateChangesetResponse, tonic::Status> {
    client
        .write()
        .create_changeset(req)
        .await
        .map(|r| r.into_inner())
}

/// `(stored inline, bytes at the live object-storage key)` of an existing object.
async fn placement(c: &TestCluster, id: &ObjectId) -> (bool, bool) {
    let row = c
        .storage
        .meta
        .get_objects(&[*id], Consistency::Strong)
        .await
        .unwrap()
        .pop()
        .flatten()
        .unwrap_or_else(|| panic!("no objects row for {id}"));
    let key = c.servers[0].ctx().objects.key(id, Area::Live);
    (
        row.inline_data.is_some(),
        c.storage.blobs.exists(&key).await.unwrap(),
    )
}

async fn row_exists(c: &TestCluster, id: &ObjectId) -> bool {
    c.storage
        .meta
        .get_objects(&[*id], Consistency::Strong)
        .await
        .unwrap()[0]
        .is_some()
}

/// SPEC 5.3 step 1: a live row without inline data has bytes at its key that hash to its id.
async fn assert_row_has_bytes(c: &TestCluster, id: &ObjectId) {
    let Some(row) = c
        .storage
        .meta
        .get_objects(&[*id], Consistency::Strong)
        .await
        .unwrap()
        .pop()
        .flatten()
    else {
        return;
    };
    if row.state != ObjectState::Live || row.inline_data.is_some() {
        return;
    }
    let key = c.servers[0].ctx().objects.key(id, Area::Live);
    let frame = c
        .storage
        .blobs
        .get(&key)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("live row {id} has no bytes at {key}"));
    let raw = zstd::decode_all(&frame[..]).unwrap();
    let actual = match id.kind() {
        Kind::Blob => ObjectId::hash_blob(&raw),
        k => ObjectId::hash_encoded(k, &raw),
    };
    assert_eq!(actual, *id, "bytes at {key} do not hash to the row's id");
}

// ---------------------------------------------------------------------------------------
// A row never exists without bytes.
// ---------------------------------------------------------------------------------------

/// A directory of `n` files whose tree encoding exceeds the inline threshold (so CreateChangeset
/// must write it to object storage).
fn big_dir_puts(dir: &str, content: &[u8], n: usize) -> Vec<(String, Vec<u8>)> {
    (0..n)
        .map(|i| {
            (
                format!("{dir}/{i:05}-{}", "p".repeat(100)),
                content.to_vec(),
            )
        })
        .collect()
}

async fn run_row_never_without_bytes(backend: Backend) {
    let c = TestCluster::start(ClusterOpts::new(backend).with_daemon(false)).await;
    let t = tag();
    let alice = c.client(Via::Server, &gh("alice")).await;
    let (root, _) = c.ref_head("main").await;
    let mut touched: Vec<ObjectId> = Vec::new();

    // Object storage fails before the write, or after it (bytes written, response lost), on
    // each object-storage call of the upload path.
    for (i, (point, action)) in [
        (
            points::blob_before("put"),
            FaultAction::Error(ErrorCode::Unavailable),
        ),
        (points::blob_after("put"), FaultAction::FailAfter),
        (points::blob_before("put"), FaultAction::Crash),
        (points::blob_after("put"), FaultAction::Crash),
    ]
    .into_iter()
    .enumerate()
    {
        let content = format!("row without bytes {i} {t}\n").into_bytes();
        let id = ObjectId::hash_blob(&content);
        touched.push(id);
        c.faults().arm(&point, action.clone(), Some(1));
        let e = try_upload(&alice, vec![(id, content.clone())])
            .await
            .unwrap_err();
        assert_eq!(
            error_from_status(&e).code,
            ErrorCode::Unavailable,
            "{point}: {e:?}"
        );
        c.faults().reset();
        assert!(
            !row_exists(&c, &id).await,
            "{point} {action:?}: a row without bytes"
        );
        // The client retries; the object is then complete.
        TestCluster::upload(&alice, &[&content]).await;
        assert_row_has_bytes(&c, &id).await;
    }

    // A whole outage, including a CreateChangeset that must write a large tree.
    let content = format!("outage {t}\n").into_bytes();
    let id = ObjectId::hash_blob(&content);
    touched.push(id);
    TestCluster::upload(&alice, &[&content]).await;
    let puts = big_dir_puts("rows/big", &content, 600);
    let puts_ref: Vec<(&str, &[u8])> = puts
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_slice()))
        .collect();
    let req = TestCluster::create_request(&root, &puts_ref, &[], 1);
    let fresh = format!("outage fresh {t}\n").into_bytes();
    let fresh_id = ObjectId::hash_blob(&fresh);
    touched.push(fresh_id);
    match c.mem() {
        Some(mem) => mem.blobs.set_down(true),
        None => c.faults().arm(
            &points::blob_before("put"),
            FaultAction::Error(ErrorCode::Unavailable),
            None,
        ),
    }
    let e = try_upload(&alice, vec![(fresh_id, fresh.clone())])
        .await
        .unwrap_err();
    assert_eq!(error_from_status(&e).code, ErrorCode::Unavailable);
    assert!(!row_exists(&c, &fresh_id).await);
    let before = c.storage.meta.children(&root).await.unwrap();
    let e = create(&alice, req.clone()).await.unwrap_err();
    assert_eq!(error_from_status(&e).code, ErrorCode::Unavailable, "{e:?}");
    assert_eq!(c.storage.meta.children(&root).await.unwrap(), before);
    match c.mem() {
        Some(mem) => mem.blobs.set_down(false),
        None => c.faults().reset(),
    }
    let created = create(&alice, req).await.unwrap();
    let cs = ObjectId::from_bytes(&created.changeset_id).unwrap();
    let tree = alice
        .query()
        .list_tree(v1::ListTreeRequest {
            changeset: idb(&cs),
            path: b"rows/big".to_vec(),
        })
        .await
        .unwrap()
        .into_inner();
    let big_tree = ObjectId::from_bytes(&tree.tree_id).unwrap();
    touched.push(big_tree);
    assert_eq!(
        placement(&c, &big_tree).await,
        (false, true),
        "large tree in object storage"
    );

    // Sweep: every live row has its bytes.
    match c.mem() {
        Some(mem) => {
            let rows = mem.meta.all_objects();
            assert!(rows.len() > touched.len());
            for r in rows {
                assert_row_has_bytes(&c, &r.id).await;
            }
        }
        None => {
            for id in &touched {
                assert_row_has_bytes(&c, id).await;
            }
        }
    }
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_objects_row_never_exists_without_bytes() {
    run_row_never_without_bytes(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_an_objects_row_never_exists_without_bytes() {
    if skip_external() {
        return;
    }
    run_row_never_without_bytes(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Inline threshold.
// ---------------------------------------------------------------------------------------

/// A directory tree whose canonical encoding is exactly `target` bytes: file entries named
/// `<prefix><index>-<padding>` (names of 100..=127 bytes, so every length varint is one byte)
/// all pointing at `blob`. Returns the entry names, the encoding and the id.
fn sized_tree(
    prefix: &str,
    target: usize,
    blob: &ObjectId,
    blob_len: u64,
) -> (Vec<Vec<u8>>, Vec<u8>, ObjectId) {
    const BASE: usize = 100;
    const MAX: usize = 127;
    let encode = |lens: &[usize]| -> (Vec<Vec<u8>>, Vec<u8>) {
        let names: Vec<Vec<u8>> = lens
            .iter()
            .enumerate()
            .map(|(i, len)| {
                let mut n = format!("{prefix}{i:05}-").into_bytes();
                n.resize(*len, b'p');
                n
            })
            .collect();
        let tree = Tree {
            entries: names
                .iter()
                .map(|n| TreeEntry {
                    name: Name::new(n).unwrap(),
                    mode: Mode::File,
                    id: *blob,
                    size: blob_len,
                })
                .collect(),
            extensions: Extensions::new(),
        };
        (names, tree.encode())
    };
    let n = target / 150;
    let mut lens = vec![BASE; n];
    let (_, base) = encode(&lens);
    assert!(base.len() <= target && base.len() + n * (MAX - BASE) >= target);
    let mut missing = target - base.len();
    for l in lens.iter_mut() {
        let add = missing.min(MAX - BASE);
        *l += add;
        missing -= add;
    }
    let (names, bytes) = encode(&lens);
    assert_eq!(bytes.len(), target, "sized tree construction");
    let id = ObjectId::hash_encoded(Kind::Tree, &bytes);
    (names, bytes, id)
}

async fn fetch_one(client: &ScionClient, id: &ObjectId) -> Vec<u8> {
    let f = TestCluster::fetch(client, &[*id], None, None)
        .await
        .unwrap();
    assert_eq!(f.items.len(), 1);
    assert_eq!(f.items[0].status, v1::ItemStatus::Ok as i32, "{id}");
    assert!(f.items[0].verified, "{id} verified");
    f.items[0].content.clone()
}

async fn run_inline_threshold(backend: Backend) {
    let c = TestCluster::start(ClusterOpts::new(backend).with_daemon(false)).await;
    assert_eq!(
        c.servers[0].config().objects.inline_threshold_bytes,
        THRESHOLD,
        "the SPEC 6.5 default threshold is in effect"
    );
    let t = tag();
    let alice = c.client(Via::Server, &gh("alice")).await;
    let (mut parent, _) = c.ref_head("main").await;
    let leaf = format!("leaf {t}\n").into_bytes();
    let leaf_id = ObjectId::hash_blob(&leaf);
    TestCluster::upload(&alice, &[&leaf]).await;
    let sizes = [THRESHOLD - 1, THRESHOLD, THRESHOLD + 1];

    // Trees: built by CreateChangeset ("c"), or uploaded first with UploadObjects ("u").
    for (n, size) in sizes.iter().enumerate() {
        for via_upload in [false, true] {
            let prefix = if via_upload { "u" } else { "c" };
            let (names, bytes, id) = sized_tree(prefix, *size, &leaf_id, leaf.len() as u64);
            if via_upload {
                let r = try_upload(&alice, vec![(id, bytes.clone())]).await.unwrap();
                assert_eq!(r.results[0].status, v1::ItemStatus::Ok as i32, "{r:?}");
                assert_eq!(
                    placement(&c, &id).await,
                    (*size <= THRESHOLD, *size > THRESHOLD),
                    "uploaded tree of {size} bytes"
                );
            }
            let dir = format!("inline/{prefix}{size}");
            let paths: Vec<String> = names
                .iter()
                .map(|nm| format!("{dir}/{}", String::from_utf8(nm.clone()).unwrap()))
                .collect();
            let puts: Vec<(&str, &[u8])> = paths
                .iter()
                .map(|p| (p.as_str(), leaf.as_slice()))
                .collect();
            let created = create(
                &alice,
                TestCluster::create_request(
                    &parent,
                    &puts,
                    &[],
                    100 + 2 * n as u32 + u32::from(via_upload),
                ),
            )
            .await
            .unwrap();
            parent = ObjectId::from_bytes(&created.changeset_id).unwrap();
            let listed = alice
                .query()
                .list_tree(v1::ListTreeRequest {
                    changeset: idb(&parent),
                    path: dir.as_bytes().to_vec(),
                })
                .await
                .unwrap()
                .into_inner();
            assert_eq!(
                ObjectId::from_bytes(&listed.tree_id).unwrap(),
                id,
                "the server built the sized tree"
            );
            assert_eq!(
                placement(&c, &id).await,
                (*size <= THRESHOLD, *size > THRESHOLD),
                "tree of {size} bytes ({prefix})"
            );
            assert_eq!(
                fetch_one(&alice, &id).await,
                bytes,
                "tree of {size} bytes reads back"
            );
        }
    }

    // Metas: the message length is tuned from a probe so the encoding hits each size.
    let probe_len = 60_000usize;
    let probe = create(&alice, {
        let content = format!("meta probe {t}\n").into_bytes();
        TestCluster::upload(&alice, &[&content]).await;
        let mut r =
            TestCluster::create_request(&parent, &[("inline/meta/probe.txt", &content)], &[], 200);
        r.meta_fields.as_mut().unwrap().message = vec![b'm'; probe_len];
        r
    })
    .await
    .unwrap();
    let probe_meta = ObjectId::from_bytes(&probe.meta_id).unwrap();
    let probe_size = fetch_one(&alice, &probe_meta).await.len();
    parent = ObjectId::from_bytes(&probe.changeset_id).unwrap();
    for (n, size) in sizes.iter().enumerate() {
        let content = format!("meta {size} {t}\n").into_bytes();
        TestCluster::upload(&alice, &[&content]).await;
        let path = format!("inline/meta/{size}.txt");
        let mut r = TestCluster::create_request(&parent, &[(&path, &content)], &[], 201 + n as u32);
        r.meta_fields.as_mut().unwrap().message = vec![b'm'; probe_len + size - probe_size];
        let created = create(&alice, r).await.unwrap();
        parent = ObjectId::from_bytes(&created.changeset_id).unwrap();
        let meta = ObjectId::from_bytes(&created.meta_id).unwrap();
        let bytes = fetch_one(&alice, &meta).await;
        assert_eq!(bytes.len(), *size, "meta encoding size");
        assert_eq!(
            placement(&c, &meta).await,
            (*size <= THRESHOLD, *size > THRESHOLD),
            "meta of {size} bytes"
        );
    }

    // Blobs are never inline, whatever their size.
    for (n, size) in sizes.iter().enumerate() {
        let content = noise(
            *size,
            1000 + n as u64 + t.len() as u64 * 7 + u64::from(t.as_bytes()[0]),
        );
        let id = ObjectId::hash_blob(&content);
        TestCluster::upload(&alice, &[&content]).await;
        assert_eq!(
            placement(&c, &id).await,
            (false, true),
            "blob of {size} bytes"
        );
        let path = format!("inline/blob/{size}.bin");
        parent =
            TestCluster::commit(&alice, &parent, &[(&path, &content)], &[], 300 + n as u32).await;
        assert_eq!(
            fetch_one(&alice, &id).await,
            content,
            "blob of {size} bytes reads back"
        );
    }
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn inline_threshold_plus_minus_one_via_rpcs() {
    run_inline_threshold(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_inline_threshold_plus_minus_one_via_rpcs() {
    if skip_external() {
        return;
    }
    run_inline_threshold(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Stale-read retry.
// ---------------------------------------------------------------------------------------

async fn run_stale_read_retry(backend: Backend) {
    let c = TestCluster::start(ClusterOpts::new(backend).with_daemon(false)).await;
    let t = tag();
    let alice = c.client(Via::Server, &gh("alice")).await;
    let (root, _) = c.ref_head("main").await;
    // Freeze the stale view: from now on stale reads see none of the writes below.
    if let Some(mem) = c.mem() {
        mem.meta.set_stale_lag(1_000_000);
        mem.meta.advance_stale_view();
    }
    let content = format!("stale read {t}\n").into_bytes();
    let id = ObjectId::hash_blob(&content);
    let cs = TestCluster::commit(&alice, &root, &[("stale/a.txt", &content)], &[], 1).await;
    if let Some(mem) = c.mem() {
        use scion_store::MetaStore;
        let stale = mem
            .meta
            .get_objects(&[id], Consistency::Stale)
            .await
            .unwrap();
        assert_eq!(stale, vec![None], "the stale view misses the new object");
        let stale = mem
            .meta
            .get_changesets(&[cs], Consistency::Stale)
            .await
            .unwrap();
        assert_eq!(stale, vec![None], "the stale view misses the new changeset");
    }
    // Every read of the new immutable rows succeeds through the strong retry.
    assert_eq!(fetch_one(&alice, &id).await, content);
    let items = alice
        .query()
        .get_changesets(v1::GetChangesetsRequest {
            ids: vec![idb(&cs)],
        })
        .await
        .unwrap()
        .into_inner()
        .items;
    assert_eq!(items[0].status, v1::ItemStatus::Ok as i32);
    let mut stream = alice
        .query()
        .read_path(v1::ReadPathRequest {
            changeset: idb(&cs),
            path: b"stale/a.txt".to_vec(),
        })
        .await
        .unwrap()
        .into_inner();
    let mut data = Vec::new();
    while let Some(chunk) = stream.message().await.unwrap() {
        data.extend_from_slice(&chunk.data);
    }
    assert_eq!(data, content);
    let has = alice
        .objects()
        .has_objects(v1::HasObjectsRequest {
            ids: vec![idb(&id)],
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(has.present, vec![true]);
    // A changeset built on the new (stale-invisible) parent.
    let next = format!("stale read next {t}\n").into_bytes();
    let cs2 = TestCluster::commit(&alice, &cs, &[("stale/b.txt", &next)], &[], 2).await;
    assert_ne!(cs2, cs);
    // Absent rows are still NOT_FOUND after the strong retry.
    let absent = ObjectId::hash_blob(format!("never stored {t}").as_bytes());
    let f = TestCluster::fetch(&alice, &[absent], None, None)
        .await
        .unwrap();
    assert_eq!(f.items[0].status, v1::ItemStatus::NotFound as i32);
    let absent_cs = ObjectId::hash_encoded(Kind::Changeset, absent.as_bytes());
    let items = alice
        .query()
        .get_changesets(v1::GetChangesetsRequest {
            ids: vec![idb(&absent_cs)],
        })
        .await
        .unwrap()
        .into_inner()
        .items;
    assert_eq!(items[0].status, v1::ItemStatus::NotFound as i32);
    if let Some(mem) = c.mem() {
        mem.meta.set_stale_lag(0);
        mem.meta.advance_stale_view();
    }
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_read_miss_is_retried_strongly() {
    run_stale_read_retry(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_stale_read_miss_is_retried_strongly() {
    if skip_external() {
        return;
    }
    run_stale_read_retry(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Orphans are harmless.
// ---------------------------------------------------------------------------------------

async fn run_orphans_harmless(backend: Backend) {
    let c = TestCluster::start(ClusterOpts::new(backend).with_daemon(false)).await;
    let t = tag();
    let alice = c.client(Via::Server, &gh("alice")).await;
    let (root, _) = c.ref_head("main").await;
    let ctx = c.servers[0].ctx();

    // (a) Correct bytes without a row (a crash between the byte write and the row insert).
    let crashed = format!("orphan from a crash {t}\n").into_bytes();
    let crashed_id = ObjectId::hash_blob(&crashed);
    c.faults().arm(
        points::UPLOAD_AFTER_BYTES,
        FaultAction::Error(ErrorCode::Unavailable),
        Some(1),
    );
    assert!(
        try_upload(&alice, vec![(crashed_id, crashed.clone())])
            .await
            .is_err()
    );
    c.faults().reset();
    // (b) Foreign bytes at the key of content that is not stored.
    let foreign = format!("foreign key content {t}\n").into_bytes();
    let foreign_id = ObjectId::hash_blob(&foreign);
    let fkey = ctx.objects.key(&foreign_id, Area::Live);
    c.storage
        .blobs
        .put(
            &fkey,
            Bytes::from(zstd::encode_all(&b"garbage, not the content"[..], 3).unwrap()),
        )
        .await
        .unwrap();
    // (c) A tree's bytes without a row.
    let (_, tree_bytes, tree_id) =
        sized_tree("o", THRESHOLD + 10, &crashed_id, crashed.len() as u64);
    c.storage
        .blobs
        .put(
            &ctx.objects.key(&tree_id, Area::Live),
            Bytes::from(zstd::encode_all(&tree_bytes[..], 3).unwrap()),
        )
        .await
        .unwrap();
    let orphans = [crashed_id, foreign_id, tree_id];
    for id in &orphans {
        assert!(
            c.storage
                .blobs
                .exists(&ctx.objects.key(id, Area::Live))
                .await
                .unwrap()
        );
        assert!(!row_exists(&c, id).await);
    }

    // No API result is affected.
    let f = TestCluster::fetch(&alice, &orphans, None, None)
        .await
        .unwrap();
    for item in &f.items {
        assert_eq!(item.status, v1::ItemStatus::NotFound as i32, "{}", item.id);
    }
    let has = alice
        .objects()
        .has_objects(v1::HasObjectsRequest {
            ids: orphans.iter().map(idb).collect(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(has.present, vec![false; 3]);
    for (path, content) in [("orphans/a.txt", &crashed), ("orphans/b.txt", &foreign)] {
        let e = create(
            &alice,
            TestCluster::create_request(&root, &[(path, content.as_slice())], &[], 1),
        )
        .await
        .unwrap_err();
        let err = error_from_status(&e);
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(err.subcode(), Some(scion_core::subcode::MISSING_BLOB));
    }
    // The SPEC 16.2 oracle (fsck with byte verification over object storage) is not
    // disturbed by orphans.
    invariants::check_16_2(&c).await.unwrap();

    // Uploading the content later yields complete, correct objects.
    TestCluster::upload(&alice, &[&crashed, &foreign]).await;
    let cs = TestCluster::commit(
        &alice,
        &root,
        &[("orphans/a.txt", &crashed), ("orphans/b.txt", &foreign)],
        &[],
        2,
    )
    .await;
    assert_eq!(fetch_one(&alice, &crashed_id).await, crashed);
    assert_eq!(fetch_one(&alice, &foreign_id).await, foreign);
    assert_row_has_bytes(&c, &foreign_id).await;
    assert_ne!(cs, root);
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn orphan_bytes_affect_no_api_result() {
    run_orphans_harmless(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_orphan_bytes_affect_no_api_result() {
    if skip_external() {
        return;
    }
    run_orphans_harmless(Backend::External).await;
}
