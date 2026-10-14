//! SPEC 19.16 failure injection and recovery (P33): one test per SPEC 16.1 failure class over
//! the real stack ([`TestCluster`]: in-process servers, daemon and fake GitHub), each followed
//! by the SPEC 16.2 oracle `invariants::check_16_2`. Every test runs on the Memory backend and,
//! as `external_*`, on CockroachDB + S3 when `SCION_TEST_CRDB_URL` and `SCION_TEST_S3_ENDPOINT`
//! are set (faults then come from the storage decorators instead of the in-memory knobs).
//!
//! 1. Metadata store down: writes UNAVAILABLE; immutable reads continue from the server's
//!    decoded LRU and the daemon cache; ref reads fail at the server while the daemon serves
//!    its TTL ref cache.
//! 2. Object storage down: cache-miss reads fail, uploads UNAVAILABLE, cached reads continue.
//! 3. Crash mid-upload: harmless orphan bytes.
//! 4. Crash at every `create.*` checkpoint: no visible row; the retry yields the same id.
//! 5. Crash after the land CAS commit: the retry returns the recorded result.
//! 6. Daemon crash: temp files deleted at startup, cache entries remain valid.
//! 7. Corrupt object at the server, the daemon and the client: detected by digest; caches
//!    discard and re-fetch; the server reports INTEGRITY and alerts.
//! 8. Client crash mid-operation: the next command completes or rolls back.
//! 9. Invalid policy reload: the last good policy is retained and the error surfaced.
//! 10. GitHub down: existing tokens work, new logins fail.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use scion_conformance::{Backend, ClusterOpts, TestCluster, Via, invariants};
use scion_core::{ErrorCode, FaultAction, ObjectId, Principal, fault::points};
use scion_object::{RefName, RepoPath};
use scion_proto::{ScionClient, error_from_status, v1};
use scion_store::{Area, Consistency};
use scion_wc::testing::{self as wct, Scenario};
use scion_wc::{WcOptions, WorkingCopy};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Every decorated metadata method (`meta.<method>.before|after`, SPEC 19.0 fault hooks).
const META_METHODS: &[&str] = &[
    "ping",
    "get_objects",
    "insert_object",
    "record_upload",
    "has_upload",
    "object_paths",
    "get_changesets",
    "changesets_by_prefix",
    "children",
    "has_root_changeset",
    "commit_changeset",
    "get_ref",
    "list_refs",
    "cas_ref",
    "ref_log",
    "get_landed",
    "get_derived",
    "put_derived",
    "delete_derived_by_source",
    "try_lease",
    "release_lease",
    "redact",
    "unredact",
    "mark_purged",
    "redaction_epoch",
    "list_redactions",
    "due_purges",
    "append_audit",
    "audit_log",
];

/// Every decorated object storage method (`blob.<method>.before|after`).
const BLOB_METHODS: &[&str] = &["put", "get", "copy", "delete", "exists", "list"];

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

/// A unique tag so contents are new to the repository on every run (External databases are
/// fresh per cluster, but object prefixes are shared per run).
fn tag() -> String {
    scion_core::ChangeId::random().to_hex()
}

fn code(s: &tonic::Status) -> ErrorCode {
    error_from_status(s).code
}

/// Poll `f` until it holds (at most 20 s).
async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn land(
    client: &ScionClient,
    r: &str,
    head: &ObjectId,
) -> Result<v1::LandResponse, tonic::Status> {
    client
        .write()
        .land(v1::LandRequest {
            r#ref: r.into(),
            head: idb(head),
            allow_empty: false,
        })
        .await
        .map(|r| r.into_inner())
}

async fn list_tree(
    client: &ScionClient,
    cs: &ObjectId,
    path: &str,
) -> Result<v1::ListTreeResponse, tonic::Status> {
    client
        .query()
        .list_tree(v1::ListTreeRequest {
            changeset: idb(cs),
            path: path.as_bytes().to_vec(),
        })
        .await
        .map(|r| r.into_inner())
}

async fn resolve(client: &ScionClient, r: &str) -> Result<(ObjectId, u64), tonic::Status> {
    let resp = client
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: r.into(),
            at_version: None,
        })
        .await?
        .into_inner();
    Ok((ObjectId::from_bytes(&resp.target).unwrap(), resp.version))
}

/// Upload one blob, returning the call's status instead of panicking.
async fn try_upload(
    client: &ScionClient,
    content: &[u8],
) -> Result<v1::UploadObjectsResponse, tonic::Status> {
    let id = ObjectId::hash_blob(content);
    client
        .write()
        .upload_objects(futures::stream::iter(vec![v1::UploadItem {
            id: idb(&id),
            kind: u32::from(scion_core::Kind::Blob.byte()),
            data: content.to_vec(),
        }]))
        .await
        .map(|r| r.into_inner())
}

fn arm_all_meta(c: &TestCluster) {
    let f = c.faults();
    for m in META_METHODS {
        f.arm(
            &points::meta_before(m),
            FaultAction::Error(ErrorCode::Unavailable),
            None,
        );
    }
}

/// Object storage unavailable: the in-memory switch, or every blob decorator point.
fn set_blobs_down(c: &TestCluster, down: bool) {
    match c.mem() {
        Some(mem) => mem.blobs.set_down(down),
        None if down => {
            for m in BLOB_METHODS {
                c.faults().arm(
                    &points::blob_before(m),
                    FaultAction::Error(ErrorCode::Unavailable),
                    None,
                );
            }
        }
        None => c.faults().disarm_all(),
    }
}

// ---------------------------------------------------------------------------------------
// Class 1: metadata store unavailable.
// ---------------------------------------------------------------------------------------

async fn run_metadata_down(backend: Backend) {
    let mut o = ClusterOpts::new(backend);
    // The daemon may cache ResolveRef for at most ref_cache_ttl_ms (SPEC 10.3); make the TTL
    // comfortably longer than the outage.
    o.daemon_tweak = Some(Arc::new(|d| d.ref_cache_ttl_ms = 600_000));
    let c = TestCluster::start(o).await;
    let t = tag();
    let alice = c.client(Via::Server, &gh("alice")).await;
    let alice_d = c.client(Via::Daemon, &gh("alice")).await;
    let (root, _) = c.ref_head("main").await;
    let a = format!("class 1 a {t}\n");
    let b = format!("class 1 b {t}\n");
    let cs = TestCluster::commit(
        &alice,
        &root,
        &[("c1/a.txt", a.as_bytes()), ("c1/b.txt", b.as_bytes())],
        &[],
        1,
    )
    .await;
    let landed = land(&alice, "main", &cs).await.unwrap();

    // Warm the server's decoded LRU (trees, changesets) and the daemon's object, access and
    // ref caches.
    let warm_root = list_tree(&alice, &cs, "").await.unwrap();
    let warm_dir = list_tree(&alice, &cs, "c1").await.unwrap();
    let dir_tree = ObjectId::from_bytes(&warm_dir.tree_id).unwrap();
    let a_id = ObjectId::hash_blob(a.as_bytes());
    let f = TestCluster::fetch(&alice_d, &[a_id, dir_tree], None, None)
        .await
        .unwrap();
    assert!(f.items.iter().all(|i| i.verified), "warm daemon fetch");
    let via_daemon = resolve(&alice_d, "main").await.unwrap();
    assert_eq!(via_daemon, (landed_target(&landed), landed.version));
    let d = c.daemon.as_ref().unwrap();
    assert!(d.handle.cache().contains(&a_id));

    arm_all_meta(&c);

    // Writes fail UNAVAILABLE, directly and through the daemon.
    let fresh = format!("class 1 new {t}\n");
    let e = try_upload(&alice, fresh.as_bytes()).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Unavailable, "upload: {e:?}");
    for client in [&alice, &alice_d] {
        let e = client
            .write()
            .create_changeset(TestCluster::create_request(
                &cs,
                &[("c1/again.txt", a.as_bytes())],
                &[],
                2,
            ))
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::Unavailable, "create: {e:?}");
        assert!(error_from_status(&e).retryable());
        let e = land(client, "main", &cs).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Unavailable, "land: {e:?}");
    }

    // Immutable reads continue from the server's decoded LRU ...
    assert_eq!(list_tree(&alice, &cs, "").await.unwrap(), warm_root);
    assert_eq!(list_tree(&alice, &cs, "c1").await.unwrap(), warm_dir);
    // ... and from the daemon cache.
    let hits = d.handle.stats().cache_hits;
    let f = TestCluster::fetch(&alice_d, &[a_id, dir_tree], None, None)
        .await
        .unwrap();
    assert!(
        f.items.iter().all(|i| i.verified),
        "daemon serves its cache"
    );
    assert_eq!(f.items[0].content, a.as_bytes());
    assert!(d.handle.stats().cache_hits >= hits + 2);

    // Ref reads fail at the server; the daemon serves its TTL cache.
    let e = resolve(&alice, "main").await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Unavailable, "server ref read: {e:?}");
    assert_eq!(resolve(&alice_d, "main").await.unwrap(), via_daemon);

    // Recovery: everything works again without a restart.
    c.faults().disarm_all();
    let cs2 = TestCluster::commit(
        &alice,
        &landed_target(&landed),
        &[("c1/after.txt", fresh.as_bytes())],
        &[],
        3,
    )
    .await;
    land(&alice, "main", &cs2).await.unwrap();
    invariants::check_16_2(&c).await.unwrap();
}

fn landed_target(r: &v1::LandResponse) -> ObjectId {
    ObjectId::from_bytes(&r.target).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn class_1_metadata_store_down() {
    run_metadata_down(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_class_1_metadata_store_down() {
    if skip_external() {
        return;
    }
    run_metadata_down(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Class 2: object storage unavailable.
// ---------------------------------------------------------------------------------------

async fn run_object_storage_down(backend: Backend) {
    let c = TestCluster::start(ClusterOpts::new(backend)).await;
    let t = tag();
    let alice = c.client(Via::Server, &gh("alice")).await;
    let alice_d = c.client(Via::Daemon, &gh("alice")).await;
    let (root, _) = c.ref_head("main").await;
    let cached = format!("class 2 cached {t}\n");
    let cold = format!("class 2 cold {t}\n");
    let cs = TestCluster::commit(
        &alice,
        &root,
        &[
            ("c2/cached.txt", cached.as_bytes()),
            ("c2/cold.txt", cold.as_bytes()),
        ],
        &[],
        1,
    )
    .await;
    let cached_id = ObjectId::hash_blob(cached.as_bytes());
    let cold_id = ObjectId::hash_blob(cold.as_bytes());
    TestCluster::fetch(&alice_d, &[cached_id], None, None)
        .await
        .unwrap();
    let warm = list_tree(&alice, &cs, "c2").await.unwrap();

    set_blobs_down(&c, true);

    // Reads over cache misses fail, at the server and through the daemon.
    for client in [&alice, &alice_d] {
        let e = TestCluster::fetch(client, &[cold_id], None, None)
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::Unavailable, "cold read: {e:?}");
    }
    // Cached data keeps serving.
    let f = TestCluster::fetch(&alice_d, &[cached_id], None, None)
        .await
        .unwrap();
    assert!(f.items[0].verified);
    assert_eq!(list_tree(&alice, &cs, "c2").await.unwrap(), warm);
    // Writes that need object storage fail UNAVAILABLE.
    let fresh = format!("class 2 new {t}\n");
    for client in [&alice, &alice_d] {
        let e = try_upload(client, fresh.as_bytes()).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Unavailable, "upload: {e:?}");
    }
    // A failed upload left no row behind (SPEC 5.3 step 1).
    let row = c
        .storage
        .meta
        .get_objects(
            &[ObjectId::hash_blob(fresh.as_bytes())],
            Consistency::Strong,
        )
        .await
        .unwrap();
    assert_eq!(row, vec![None]);

    set_blobs_down(&c, false);
    let f = TestCluster::fetch(&alice, &[cold_id], None, None)
        .await
        .unwrap();
    assert!(f.items[0].verified, "recovered without a restart");
    TestCluster::upload(&alice, &[fresh.as_bytes()]).await;
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn class_2_object_storage_down() {
    run_object_storage_down(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_class_2_object_storage_down() {
    if skip_external() {
        return;
    }
    run_object_storage_down(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Class 3: server crash mid-upload.
// ---------------------------------------------------------------------------------------

async fn run_crash_mid_upload(backend: Backend) {
    let mut c = TestCluster::start(ClusterOpts::new(backend).with_daemon(false)).await;
    let t = tag();
    let (root, _) = c.ref_head("main").await;
    let orphan = format!("class 3 orphan {t}\n");
    let orphan_id = ObjectId::hash_blob(orphan.as_bytes());
    let key = c.servers[0].ctx().objects.key(&orphan_id, Area::Live);

    {
        let alice = c.client(Via::Server, &gh("alice")).await;
        c.faults()
            .arm(points::UPLOAD_AFTER_BYTES, FaultAction::Crash, Some(1));
        let e = try_upload(&alice, orphan.as_bytes()).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Unavailable, "{e:?}");
    }
    c.faults().reset();
    c.restart_server(0).await;
    let alice = c.client(Via::Server, &gh("alice")).await;

    // Bytes without a row: an orphan.
    assert!(
        c.storage.blobs.exists(&key).await.unwrap(),
        "bytes were written"
    );
    let row = c
        .storage
        .meta
        .get_objects(&[orphan_id], Consistency::Strong)
        .await
        .unwrap();
    assert_eq!(row, vec![None], "no objects row after the crash");
    // The orphan affects no API result.
    let has = alice
        .objects()
        .has_objects(v1::HasObjectsRequest {
            ids: vec![idb(&orphan_id)],
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(has.present, vec![false]);
    let f = TestCluster::fetch(&alice, &[orphan_id], None, None)
        .await
        .unwrap();
    assert_eq!(f.items[0].status, v1::ItemStatus::NotFound as i32);
    let e = alice
        .write()
        .create_changeset(TestCluster::create_request(
            &root,
            &[("c3/orphan.txt", orphan.as_bytes())],
            &[],
            1,
        ))
        .await
        .unwrap_err();
    let err = error_from_status(&e);
    assert_eq!(err.code, ErrorCode::InvalidArgument, "{err:?}");
    assert_eq!(err.subcode(), Some(scion_core::subcode::MISSING_BLOB));
    invariants::check_16_2(&c).await.unwrap();

    // A crash after the row: the row implies durable bytes.
    let second = format!("class 3 after row {t}\n");
    let second_id = ObjectId::hash_blob(second.as_bytes());
    c.faults()
        .arm(points::UPLOAD_AFTER_ROW, FaultAction::Crash, Some(1));
    assert!(try_upload(&alice, second.as_bytes()).await.is_err());
    c.faults().reset();
    c.restart_server(0).await;
    let alice = c.client(Via::Server, &gh("alice")).await;
    let row = c
        .storage
        .meta
        .get_objects(&[second_id], Consistency::Strong)
        .await
        .unwrap();
    assert!(row[0].is_some(), "the row was committed");
    let k2 = c.servers[0].ctx().objects.key(&second_id, Area::Live);
    assert!(
        c.storage.blobs.exists(&k2).await.unwrap(),
        "row implies bytes"
    );

    // The client retries: both uploads succeed and the content is usable.
    TestCluster::upload(&alice, &[orphan.as_bytes(), second.as_bytes()]).await;
    let cs = TestCluster::commit(
        &alice,
        &root,
        &[
            ("c3/orphan.txt", orphan.as_bytes()),
            ("c3/second.txt", second.as_bytes()),
        ],
        &[],
        1,
    )
    .await;
    let f = TestCluster::fetch(&alice, &[orphan_id, second_id], None, None)
        .await
        .unwrap();
    assert!(f.items.iter().all(|i| i.verified));
    assert_ne!(cs, root);
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn class_3_crash_mid_upload_leaves_harmless_orphans() {
    run_crash_mid_upload(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_class_3_crash_mid_upload_leaves_harmless_orphans() {
    if skip_external() {
        return;
    }
    run_crash_mid_upload(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Class 4: server crash mid-CreateChangeset.
// ---------------------------------------------------------------------------------------

const CREATE_POINTS: &[&str] = &[
    points::CREATE_AFTER_VALIDATE,
    points::CREATE_AFTER_TREES,
    points::CREATE_AFTER_META,
    points::CREATE_AFTER_DERIVE,
    points::CREATE_AFTER_COMMIT,
];

async fn run_crash_mid_create(backend: Backend) {
    let mut c = TestCluster::start(ClusterOpts::new(backend).with_daemon(false)).await;
    let t = tag();
    let (parent, _) = c.ref_head("main").await;
    for (i, point) in CREATE_POINTS.iter().enumerate() {
        let content = format!("class 4 {point} {t}\n");
        let path = format!("c4/{i}/deep/file.txt");
        let req = TestCluster::create_request(
            &parent,
            &[(&path, content.as_bytes())],
            &[],
            40 + i as u32,
        );
        let alice = c.client(Via::Server, &gh("alice")).await;
        TestCluster::upload(&alice, &[content.as_bytes()]).await;
        let before = c.storage.meta.children(&parent).await.unwrap();
        c.faults().arm(point, FaultAction::Crash, Some(1));
        let e = alice
            .write()
            .create_changeset(req.clone())
            .await
            .unwrap_err();
        assert_eq!(code(&e), ErrorCode::Unavailable, "{point}: {e:?}");
        assert!(c.faults().hits(point) > 0, "{point} reached");
        c.faults().reset();
        c.restart_server(0).await;
        let mid = c.storage.meta.children(&parent).await.unwrap();
        // The SPEC 16.2 oracle on the crashed state (before any retry).
        invariants::check_16_2(&c)
            .await
            .unwrap_or_else(|e| panic!("{point}: {e}"));

        let alice = c.client(Via::Server, &gh("alice")).await;
        let id = ObjectId::from_bytes(
            &alice
                .write()
                .create_changeset(req.clone())
                .await
                .unwrap()
                .into_inner()
                .changeset_id,
        )
        .unwrap();
        if *point == points::CREATE_AFTER_COMMIT {
            assert!(
                mid.contains(&id),
                "{point}: the committed row survives the crash"
            );
        } else {
            assert_eq!(mid, before, "{point}: no changeset row became visible");
            assert!(!mid.contains(&id));
        }
        // The retry and a second retry yield the same id; one row exists.
        let again = alice
            .write()
            .create_changeset(req)
            .await
            .unwrap()
            .into_inner();
        assert_eq!(ObjectId::from_bytes(&again.changeset_id).unwrap(), id);
        let after = c.storage.meta.children(&parent).await.unwrap();
        assert_eq!(after.iter().filter(|x| **x == id).count(), 1);
        let rows = c
            .storage
            .meta
            .get_changesets(&[id], Consistency::Strong)
            .await
            .unwrap();
        assert!(rows[0].is_some());
    }
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn class_4_crash_mid_create_changeset() {
    run_crash_mid_create(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_class_4_crash_mid_create_changeset() {
    if skip_external() {
        return;
    }
    run_crash_mid_create(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Class 5: server crash after the CAS commit, before the response.
// ---------------------------------------------------------------------------------------

async fn run_crash_after_cas(backend: Backend) {
    let mut c = TestCluster::start(ClusterOpts::new(backend).with_daemon(false)).await;
    let t = tag();
    for (round, needs_rebase) in [(0u32, false), (1u32, true)] {
        let alice = c.client(Via::Server, &gh("alice")).await;
        let (tip, version) = c.ref_head("main").await;
        let content = format!("class 5 round {round} {t}\n");
        let mine = TestCluster::commit(
            &alice,
            &tip,
            &[(&format!("c5/{round}/mine.txt"), content.as_bytes())],
            &[],
            50 + round,
        )
        .await;
        let mut version = version;
        if needs_rebase {
            // Someone else lands first, so the crashed land rebases `mine`.
            let other = format!("class 5 other {t}\n");
            let theirs =
                TestCluster::commit(&alice, &tip, &[("c5/other.txt", other.as_bytes())], &[], 59)
                    .await;
            version = land(&alice, "main", &theirs).await.unwrap().version;
        }
        c.faults()
            .arm(points::LAND_AFTER_COMMIT, FaultAction::Crash, Some(1));
        let e = land(&alice, "main", &mine).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Unavailable, "{e:?}");
        c.faults().reset();
        c.restart_server(0).await;

        // The CAS committed: the ref moved by exactly one version.
        let (moved_to, v) = c.ref_head("main").await;
        assert_eq!(v, version + 1, "round {round}: the CAS committed");
        let alice = c.client(Via::Server, &gh("alice")).await;
        let r1 = land(&alice, "main", &mine).await.unwrap();
        assert_eq!(
            landed_target(&r1),
            moved_to,
            "round {round}: recorded result"
        );
        assert_eq!(
            r1.version, v,
            "round {round}: the retry did not move the ref"
        );
        // The landed index holds the recorded result of the crashed attempt (SPEC 8.4).
        let change = scion_core::ChangeId(TestCluster::change_id(50 + round).try_into().unwrap());
        let rec = c
            .storage
            .meta
            .get_landed(&RefName::main(), &change)
            .await
            .unwrap()
            .expect("the landed row of the crashed attempt");
        assert_eq!((rec.source, rec.result, rec.version), (mine, moved_to, v));
        // A reported entry (if any) is the recorded one.
        for e in &r1.landed {
            assert_eq!(
                (e.source.clone(), e.result.clone()),
                (idb(&mine), idb(&moved_to))
            );
        }
        if needs_rebase {
            assert_ne!(moved_to, mine, "the member was rebased");
        } else {
            assert_eq!(moved_to, mine, "fast-forward");
        }
        let r2 = land(&alice, "main", &mine).await.unwrap();
        assert_eq!((r2.target, r2.version), (r1.target.clone(), r1.version));
        assert_eq!(c.ref_head("main").await, (moved_to, v));
        invariants::check_16_2(&c).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn class_5_crash_after_cas_commit() {
    run_crash_after_cas(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_class_5_crash_after_cas_commit() {
    if skip_external() {
        return;
    }
    run_crash_after_cas(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Class 6: daemon crash.
// ---------------------------------------------------------------------------------------

async fn run_daemon_crash(backend: Backend) {
    let mut c = TestCluster::start(ClusterOpts::new(backend)).await;
    let t = tag();
    let alice = c.client(Via::Server, &gh("alice")).await;
    let (root, _) = c.ref_head("main").await;
    let contents: Vec<String> = (0..4).map(|i| format!("class 6 file {i} {t}\n")).collect();
    let puts: Vec<(String, &[u8])> = contents
        .iter()
        .enumerate()
        .map(|(i, s)| (format!("c6/f{i}.txt"), s.as_bytes()))
        .collect();
    let puts_ref: Vec<(&str, &[u8])> = puts.iter().map(|(p, c)| (p.as_str(), *c)).collect();
    TestCluster::commit(&alice, &root, &puts_ref, &[], 1).await;
    let ids: Vec<ObjectId> = contents
        .iter()
        .map(|s| ObjectId::hash_blob(s.as_bytes()))
        .collect();
    {
        let alice_d = c.client(Via::Daemon, &gh("alice")).await;
        let f = TestCluster::fetch(&alice_d, &ids, None, None)
            .await
            .unwrap();
        assert!(f.items.iter().all(|i| i.verified));
    }
    let d = c.daemon.as_ref().unwrap();
    for id in &ids {
        assert!(d.handle.cache().contains(id), "cached before the crash");
    }
    // Leftovers of inserts interrupted by the crash (SPEC 10.2: temp file, then rename).
    let shard = d.handle.cache().entry_path_of(&ids[0]);
    let shard = shard.parent().unwrap().to_path_buf();
    let temps = [
        shard.join(".tmp-crashed-insert"),
        shard.join(".tmp-0123456789"),
    ];
    for p in &temps {
        std::fs::write(p, b"partial write").unwrap();
    }

    let d = c.daemon.as_mut().unwrap();
    d.kill().await;
    d.restart().await;
    let d = c.daemon.as_ref().unwrap();
    for p in &temps {
        assert!(!p.exists(), "temp file {p:?} deleted at startup");
    }
    for id in &ids {
        assert!(d.handle.cache().contains(id), "entries remain valid");
    }
    let before = d.handle.stats();
    let alice_d = c.client(Via::Daemon, &gh("alice")).await;
    let f = TestCluster::fetch(&alice_d, &ids, None, None)
        .await
        .unwrap();
    assert!(f.items.iter().all(|i| i.verified));
    for (item, s) in f.items.iter().zip(&contents) {
        assert_eq!(item.content, s.as_bytes());
    }
    let after = d.handle.stats();
    assert!(
        after.cache_hits >= before.cache_hits + ids.len() as u64,
        "served from the surviving cache: {before:?} -> {after:?}"
    );
    assert_eq!(
        after.cache_misses, before.cache_misses,
        "no entry was re-fetched"
    );
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn class_6_daemon_crash() {
    run_daemon_crash(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_class_6_daemon_crash() {
    if skip_external() {
        return;
    }
    run_daemon_crash(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Class 7: corrupt object at every tier.
// ---------------------------------------------------------------------------------------

fn metric(text: &str, name: &str, labels: &[(&str, &str)]) -> f64 {
    TestCluster::metric(text, name, labels).unwrap_or(0.0)
}

fn flip_last_byte(path: &std::path::Path) -> Vec<u8> {
    let original = std::fs::read(path).unwrap();
    let mut bad = original.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0x01;
    std::fs::write(path, &bad).unwrap();
    original
}

async fn run_corrupt_objects(backend: Backend) {
    let c = TestCluster::start(ClusterOpts::new(backend)).await;
    let t = tag();
    let alice = c.client(Via::Server, &gh("alice")).await;
    let alice_d = c.client(Via::Daemon, &gh("alice")).await;
    let (root, _) = c.ref_head("main").await;
    let server_copy = format!("class 7 server copy {t}\n");
    let daemon_copy = format!("class 7 daemon copy {t}\n");
    let client_file = format!("class 7 client {t}\n");
    let cs = TestCluster::commit(
        &alice,
        &root,
        &[
            ("c7/server.txt", server_copy.as_bytes()),
            ("c7/daemon.txt", daemon_copy.as_bytes()),
            ("c7/client/file.txt", client_file.as_bytes()),
        ],
        &[],
        1,
    )
    .await;
    land(&alice, "main", &cs).await.unwrap();

    // --- The server's own copy: INTEGRITY, counter, metric and alert event. ---
    let sid = ObjectId::hash_blob(server_copy.as_bytes());
    let key = c.servers[0].ctx().objects.key(&sid, Area::Live);
    let good = c.storage.blobs.get(&key).await.unwrap().unwrap();
    match c.mem() {
        Some(mem) => mem.blobs.corrupt(&key),
        None => c
            .storage
            .blobs
            .put(
                &key,
                Bytes::from(zstd::encode_all(&b"not the content"[..], 3).unwrap()),
            )
            .await
            .unwrap(),
    }
    let m0 = c.server_metrics(0).await;
    let counters0 = c.counters();
    let e = TestCluster::fetch(&alice, &[sid], None, None)
        .await
        .unwrap_err();
    let err = error_from_status(&e);
    assert_eq!(err.code, ErrorCode::Integrity, "{err:?}");
    assert!(err.retryable(), "INTEGRITY is retryable (SPEC 7.2)");
    assert!((c.counters() - counters0).integrity_failures >= 1);
    let m1 = c.server_metrics(0).await;
    assert!(
        metric(&m1, "scion_integrity_failures_total", &[("tier", "server")])
            > metric(&m0, "scion_integrity_failures_total", &[("tier", "server")]),
        "scion_integrity_failures_total{{tier=server}} moved"
    );
    let text = sid.to_text();
    assert!(
        c.logs
            .events()
            .iter()
            .any(|ev| ev["event"] == "integrity_alert" && ev["object"] == text.as_str()),
        "an integrity_alert event names the object"
    );
    // Through the daemon the corrupt server copy is never served or cached either.
    assert!(
        TestCluster::fetch(&alice_d, &[sid], None, None)
            .await
            .is_err()
    );
    assert!(!c.daemon.as_ref().unwrap().handle.cache().contains(&sid));
    // The operator restores the bytes; reads resume.
    c.storage.blobs.put(&key, good).await.unwrap();
    let f = TestCluster::fetch(&alice, &[sid], None, None)
        .await
        .unwrap();
    assert!(f.items[0].verified);

    // --- The daemon's cached copy: discarded and re-fetched upstream. ---
    let did = ObjectId::hash_blob(daemon_copy.as_bytes());
    let f = TestCluster::fetch(&alice_d, &[did], None, None)
        .await
        .unwrap();
    assert!(f.items[0].verified);
    let d = c.daemon.as_ref().unwrap();
    let path = d.handle.cache().entry_path_of(&did);
    let original = flip_last_byte(&path);
    let dm0 = c.daemon_metrics();
    let ups0 = d.handle.stats().upstream_requests;
    let f = TestCluster::fetch(&alice_d, &[did], None, None)
        .await
        .unwrap();
    assert!(f.items[0].verified, "the daemon re-fetched a good copy");
    assert_eq!(f.items[0].content, daemon_copy.as_bytes());
    assert!(
        d.handle.stats().upstream_requests > ups0,
        "re-fetched upstream"
    );
    let dm1 = c.daemon_metrics();
    assert!(
        metric(
            &dm1,
            "scion_integrity_failures_total",
            &[("tier", "daemon")]
        ) > metric(
            &dm0,
            "scion_integrity_failures_total",
            &[("tier", "daemon")]
        ),
        "the daemon counted the corrupt entry"
    );
    assert!(
        d.handle.cache().contains(&did),
        "the good copy is cached again"
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);

    // --- The client's local tree cache: discarded and re-fetched. ---
    let s = Scenario::new(c.client(Via::Daemon, &gh("alice")).await).unwrap();
    let dir = RepoPath::parse(b"c7").unwrap();
    let wc_root = s.fresh_dir("corrupt");
    let mut wc = WorkingCopy::checkout(
        s.client.clone(),
        &wc_root,
        "main",
        vec![dir.clone()],
        WcOptions::default(),
    )
    .await
    .unwrap();
    assert!(wc.status().await.unwrap().is_clean());
    let listing = list_tree(&alice, &cs, "c7/client").await.unwrap();
    let tree = ObjectId::from_bytes(&listing.tree_id).unwrap();
    let hex = tree.hex();
    let cached = wc_root
        .join(".scion")
        .join("cache")
        .join("objects")
        .join("t")
        .join(&hex[..2])
        .join(&hex);
    assert!(cached.exists(), "the client cached the tree at {cached:?}");
    drop(wc);
    let original = flip_last_byte(&cached);
    let mut wc = WorkingCopy::open(&wc_root, s.client.clone(), WcOptions::default()).unwrap();
    let calls = s.client.calls();
    // The digest mismatch is detected on read, the entry discarded and the tree re-fetched
    // within the same command (SPEC 16.1 class 7 at the client tier; amendment W6-3).
    let st = wc.status().await.unwrap();
    assert!(st.is_clean(), "status re-fetches the corrupt tree: {st:?}");
    assert!(s.client.calls() > calls, "the corrupt tree was re-fetched");
    assert_eq!(
        std::fs::read(&cached).unwrap(),
        original,
        "a good copy replaced it"
    );
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn class_7_corrupt_object_at_every_tier() {
    run_corrupt_objects(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_class_7_corrupt_object_at_every_tier() {
    if skip_external() {
        return;
    }
    run_corrupt_objects(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Class 8: client crash mid-operation.
// ---------------------------------------------------------------------------------------

async fn create_ref(c: &TestCluster, name: &str, target: &ObjectId) {
    let admin = c.client(Via::Server, &gh("admin")).await;
    admin
        .refs()
        .create_ref(v1::CreateRefRequest {
            name: name.into(),
            target: idb(target),
            reason: "failure suite".into(),
        })
        .await
        .unwrap();
}

async fn run_client_crash(backend: Backend) {
    let mut o = ClusterOpts::new(backend);
    // The write scenarios resolve the ref right after landing on it; a daemon TTL ref cache
    // may legitimately answer with the previous target within its bound (SPEC 10.3), so the
    // daemon run disables it.
    o.daemon_tweak = Some(Arc::new(|d| d.ref_cache_ttl_ms = 0));
    let c = TestCluster::start(o).await;
    let t = tag();
    let alice = c.client(Via::Server, &gh("alice")).await;
    let (root, _) = c.ref_head("main").await;
    let a1 = format!("a v1 {t}\n");
    let a2 = format!("a v2 {t}\n");
    let keep = format!("keep {t}\n");
    let fa = TestCluster::commit(
        &alice,
        &root,
        &[
            ("c8/a.txt", a1.as_bytes()),
            ("c8/keep.txt", keep.as_bytes()),
            ("c8/gone.txt", b"gone\n"),
        ],
        &[],
        1,
    )
    .await;
    let fb = TestCluster::commit(
        &alice,
        &fa,
        &[("c8/a.txt", a2.as_bytes()), ("c8/sub/new.txt", b"new\n")],
        &["c8/gone.txt"],
        2,
    )
    .await;
    create_ref(&c, "crash-a", &fa).await;
    create_ref(&c, "crash-b", &fb).await;
    create_ref(&c, "crash-w", &fb).await;
    for via in Via::ALL {
        let s = Scenario::new(c.client(via, &gh("alice")).await).unwrap();
        for p in wct::materialization_points() {
            wct::goto_crash_recovery(&s, "crash-a", "crash-b", p)
                .await
                .unwrap_or_else(|e| panic!("{}: goto crash at {p}: {e}", via.as_str()));
        }
        let r = RefName::parse("crash-w").unwrap();
        let outcomes = wct::crash_recovery_all(&s, &r)
            .await
            .unwrap_or_else(|e| panic!("{}: write crash recovery: {e}", via.as_str()));
        assert_eq!(outcomes.len(), wct::write_points().len());
    }
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn class_8_client_crash_mid_operation() {
    run_client_crash(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_class_8_client_crash_mid_operation() {
    if skip_external() {
        return;
    }
    run_client_crash(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Class 9: invalid policy reload.
// ---------------------------------------------------------------------------------------

async fn can_read(client: &ScionClient, cs: &ObjectId) -> Result<(), tonic::Status> {
    client
        .query()
        .get_changesets(v1::GetChangesetsRequest { ids: vec![idb(cs)] })
        .await
        .map(|_| ())
}

async fn run_invalid_policy(backend: Backend) {
    let c = TestCluster::start(ClusterOpts::new(backend).with_servers(2)).await;
    let (root, _) = c.ref_head("main").await;
    let alice = c.client(Via::Server, &gh("alice")).await;
    let mallory = c.client(Via::Server, &gh("mallory")).await;
    can_read(&alice, &root).await.unwrap();
    assert_eq!(
        code(&can_read(&mallory, &root).await.unwrap_err()),
        ErrorCode::PermissionDenied
    );
    let epochs: Vec<u64> = c.servers.iter().map(|s| s.ctx().policy.epoch()).collect();
    let r = alice
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap();
    let trailer_epoch = TestCluster::epochs(r.metadata()).unwrap().policy;

    // An invalid policy: the error is surfaced, every server keeps its last good policy.
    let invalid =
        "version = 1\n[[paths]]\nprefix = \"no-leading-slash\"\nread = [\"github:mallory\"]\n";
    let e = c.set_policy(invalid).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e:?}");
    let still: Vec<u64> = c.servers.iter().map(|s| s.ctx().policy.epoch()).collect();
    assert_eq!(still, epochs, "the last good policy is retained");
    can_read(&alice, &root).await.unwrap();
    assert_eq!(
        code(&can_read(&mallory, &root).await.unwrap_err()),
        ErrorCode::PermissionDenied,
        "the invalid policy did not take effect"
    );
    let r = alice
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap();
    assert_eq!(
        TestCluster::epochs(r.metadata()).unwrap().policy,
        trailer_epoch
    );
    assert!(
        c.logs
            .raw()
            .contains("invalid policy reload; keeping last known good policy"),
        "the reload error is logged"
    );
    // A later valid policy applies.
    let valid = TestCluster::DEFAULT_ACL.replace("eng = [", "eng = [\"github:mallory\", ");
    c.set_policy(&valid).unwrap();
    eventually("mallory gains read", || {
        c.servers
            .iter()
            .all(|s| s.ctx().policy.epoch() != epochs[0])
    })
    .await;
    can_read(&mallory, &root).await.unwrap();
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn class_9_invalid_policy_reload() {
    run_invalid_policy(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_class_9_invalid_policy_reload() {
    if skip_external() {
        return;
    }
    run_invalid_policy(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Class 10: GitHub unavailable.
// ---------------------------------------------------------------------------------------

fn github_login(token: &str) -> v1::LoginRequest {
    v1::LoginRequest {
        credential: Some(v1::login_request::Credential::GithubAccessToken(
            token.to_owned(),
        )),
    }
}

async fn run_github_down(backend: Backend) {
    let c = TestCluster::start(ClusterOpts::new(backend)).await;
    let t = tag();
    let gh_token = format!("gho_failures_{t}");
    c.github.add_user(&gh_token, "bob", 4242);
    let anon = c.client_with_token(Via::Server, None).await;
    let token = anon
        .auth()
        .login(github_login(&gh_token))
        .await
        .unwrap()
        .into_inner()
        .token;
    let (root, _) = c.ref_head("main").await;

    c.github.set_down(true);
    for via in Via::ALL {
        let anon = c.client_with_token(via, None).await;
        let e = anon
            .auth()
            .login(github_login(&gh_token))
            .await
            .unwrap_err();
        let err = error_from_status(&e);
        assert_eq!(
            err.code,
            ErrorCode::Unavailable,
            "{}: {err:?}",
            via.as_str()
        );
        assert!(err.retryable());
        // Existing tokens keep working (verification is local; SPEC 12.1).
        let bob = c.client_with_token(via, Some(token.clone())).await;
        let who = bob
            .auth()
            .who_am_i(v1::WhoAmIRequest {})
            .await
            .unwrap()
            .into_inner();
        assert_eq!(who.principal, "github:bob");
        can_read(&bob, &root).await.unwrap();
    }
    // Service logins do not depend on GitHub.
    let (name, secret) = TestCluster::SERVICES[0];
    anon.auth()
        .login(v1::LoginRequest {
            credential: Some(v1::login_request::Credential::Service(
                v1::ServiceCredential {
                    service_name: name.into(),
                    service_secret: secret.into(),
                },
            )),
        })
        .await
        .unwrap();

    c.github.set_down(false);
    anon.auth().login(github_login(&gh_token)).await.unwrap();
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn class_10_github_down() {
    run_github_down(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_class_10_github_down() {
    if skip_external() {
        return;
    }
    run_github_down(Backend::External).await;
}
