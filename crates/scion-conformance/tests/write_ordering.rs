//! SPEC 19.3 write ordering and SPEC 19.6 eager values under faults, via RPC (P33).
//!
//! For each write operation (UploadObjects, CreateChangeset, Land with a rebase), the suite
//! first discovers which fault points the operation reaches: every SPEC 5.3 / land checkpoint
//! (`scion_core::fault::points::ALL`) and every storage decorator point
//! (`meta.<method>.before|after`, `blob.<method>.before|after`). It then injects, at every
//! reached point, a crash (the server process dies there and is restarted over the same
//! storage) and, at every decorator `after` point, a lost response (`FailAfter`: the effect
//! happened, the caller sees UNAVAILABLE). After each injection:
//!
//! - the SPEC 16.2 oracle holds (`invariants::check_16_2`: fsck with every eager type required
//!   and byte verification, so no visible changeset violates SPEC 4.5; ref-log provenance);
//! - every visible changeset has its eager values (SPEC 19.6), checked explicitly too;
//! - every live `objects` row has durable bytes (SPEC 5.3 step 1);
//! - the client's retry succeeds with the same result (same changeset id; the recorded land
//!   result).
//!
//! Runs on the Memory backend and, as `external_*`, on CockroachDB + S3 when configured.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scion_conformance::{Backend, ClusterOpts, TestCluster, Via, invariants};
use scion_core::{ChangeId, FaultAction, Kind, Mode, ObjectId, Principal, fault::points};
use scion_derived::history;
use scion_object::{Canonical, Extensions, Name, RefName, Tree, TreeEntry};
use scion_proto::{ScionClient, v1};
use scion_store::{Area, Consistency, ObjectState};
use std::sync::Arc;

/// Every decorated metadata method.
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

/// Every decorated object storage method.
const BLOB_METHODS: &[&str] = &["put", "get", "copy", "delete", "exists", "list"];

/// Every fault point of SPEC 19.0: checkpoints and decorator points.
fn all_points() -> Vec<String> {
    let mut out: Vec<String> = points::ALL.iter().map(|p| (*p).to_owned()).collect();
    for m in META_METHODS {
        out.push(points::meta_before(m));
        out.push(points::meta_after(m));
    }
    for m in BLOB_METHODS {
        out.push(points::blob_before(m));
        out.push(points::blob_after(m));
    }
    out
}

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
    ChangeId::random().to_hex()
}

/// No daemon; the redaction refresher is slowed down so background storage calls do not
/// interleave with the operation under test.
fn opts(backend: Backend) -> ClusterOpts {
    let mut o = ClusterOpts::new(backend).with_daemon(false);
    let base = o.server.tweak.clone();
    o.server.tweak = Some(Arc::new(move |cfg| {
        if let Some(b) = &base {
            b(cfg);
        }
        cfg.redaction.propagation_bound_ms = 3_600_000;
    }));
    o
}

/// The fault injected at a point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Injection {
    /// The server dies at the point and is restarted over the same storage.
    Crash,
    /// The decorated effect happened but the response is lost (UNAVAILABLE).
    LostResponse,
}

/// The injections tried at `point`.
fn injections(point: &str) -> Vec<Injection> {
    let decorator_after =
        (point.starts_with("meta.") || point.starts_with("blob.")) && point.ends_with(".after");
    if decorator_after {
        vec![Injection::Crash, Injection::LostResponse]
    } else {
        vec![Injection::Crash]
    }
}

/// Points reached by `run`, measured with the hit counters.
async fn reached<F, Fut>(c: &TestCluster, run: F) -> Vec<String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let f = c.faults();
    f.reset_hits();
    run().await;
    let out: Vec<String> = all_points().into_iter().filter(|p| f.hits(p) > 0).collect();
    eprintln!("reached {} fault points: {out:?}", out.len());
    out
}

/// Arm `inj` at `point` for one firing.
fn arm(c: &TestCluster, point: &str, inj: Injection) {
    let action = match inj {
        Injection::Crash => FaultAction::Crash,
        Injection::LostResponse => FaultAction::FailAfter,
    };
    c.faults().arm(point, action, Some(1));
}

/// After an attempt: disarm, and restart the server after a crash.
async fn recover(c: &mut TestCluster, inj: Injection) {
    c.faults().reset();
    if inj == Injection::Crash {
        c.restart_server(0).await;
    }
}

/// SPEC 5.3 step 1: a live row without inline data has bytes at its key hashing to its id.
async fn assert_row_has_bytes(c: &TestCluster, id: &ObjectId, what: &str) {
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
        .unwrap_or_else(|| panic!("{what}: live row {id} without bytes"));
    let raw = zstd::decode_all(&frame[..]).unwrap();
    let actual = match id.kind() {
        Kind::Blob => ObjectId::hash_blob(&raw),
        k => ObjectId::hash_encoded(k, &raw),
    };
    assert_eq!(actual, *id, "{what}: bytes do not match the row's id");
}

/// SPEC 19.6: every changeset visible as a child of `parent` has its eager `history` value.
async fn assert_eager_for_children(c: &TestCluster, parent: &ObjectId, what: &str) {
    for child in c.storage.meta.children(parent).await.unwrap() {
        let row = c
            .storage
            .meta
            .get_derived(&history::derived_key(&child), Consistency::Strong)
            .await
            .unwrap();
        assert!(
            row.is_some(),
            "{what}: visible changeset {child} lacks its eager history value"
        );
    }
}

async fn invariants_hold(c: &TestCluster, what: &str) {
    invariants::check_16_2(c)
        .await
        .unwrap_or_else(|e| panic!("{what}: {e}"));
}

// ---------------------------------------------------------------------------------------
// UploadObjects.
// ---------------------------------------------------------------------------------------

/// A blob and a small (inline) tree referencing it: both object write paths of SPEC 5.3
/// step 1.
fn upload_items(n: usize, t: &str) -> Vec<(ObjectId, Vec<u8>)> {
    let content = format!("write ordering upload {n} {t}\n").into_bytes();
    let blob = ObjectId::hash_blob(&content);
    let tree = Tree {
        entries: vec![TreeEntry {
            name: Name::new(format!("f{n}").as_bytes()).unwrap(),
            mode: Mode::File,
            id: blob,
            size: content.len() as u64,
        }],
        extensions: Extensions::new(),
    };
    let bytes = tree.encode();
    vec![
        (blob, content),
        (ObjectId::hash_encoded(Kind::Tree, &bytes), bytes),
    ]
}

async fn upload(client: &ScionClient, items: &[(ObjectId, Vec<u8>)]) -> Result<(), tonic::Status> {
    let msgs: Vec<v1::UploadItem> = items
        .iter()
        .map(|(id, data)| v1::UploadItem {
            id: idb(id),
            kind: u32::from(id.kind().byte()),
            data: data.clone(),
        })
        .collect();
    let r = client
        .write()
        .upload_objects(futures::stream::iter(msgs))
        .await?
        .into_inner();
    for res in &r.results {
        assert_eq!(res.status, v1::ItemStatus::Ok as i32, "{res:?}");
    }
    Ok(())
}

async fn run_upload(backend: Backend) {
    let mut c = TestCluster::start(opts(backend)).await;
    let t = tag();
    let mut n = 0usize;
    let alice = c.client(Via::Server, &gh("alice")).await;
    let items = upload_items(n, &t);
    let pts = reached(&c, || async { upload(&alice, &items).await.unwrap() }).await;
    for want in [
        points::UPLOAD_AFTER_BYTES.to_owned(),
        points::UPLOAD_AFTER_ROW.to_owned(),
        points::blob_before("put"),
        points::blob_after("put"),
        points::meta_before("insert_object"),
        points::meta_after("insert_object"),
    ] {
        assert!(pts.contains(&want), "UploadObjects reaches {want}: {pts:?}");
    }
    let mut runs = 0;
    for p in &pts {
        for inj in injections(p) {
            n += 1;
            runs += 1;
            let what = format!("upload {p} {inj:?}");
            let items = upload_items(n, &t);
            let alice = c.client(Via::Server, &gh("alice")).await;
            arm(&c, p, inj);
            let _ = upload(&alice, &items).await;
            recover(&mut c, inj).await;
            for (id, _) in &items {
                assert_row_has_bytes(&c, id, &what).await;
            }
            invariants_hold(&c, &what).await;
            // The client retries; the objects are complete and stored once.
            let alice = c.client(Via::Server, &gh("alice")).await;
            upload(&alice, &items)
                .await
                .unwrap_or_else(|e| panic!("{what}: retry: {e:?}"));
            for (id, _) in &items {
                assert_row_has_bytes(&c, id, &what).await;
                let rows = c
                    .storage
                    .meta
                    .get_objects(&[*id], Consistency::Strong)
                    .await
                    .unwrap();
                assert!(rows[0].is_some(), "{what}: the retry stored {id}");
            }
        }
    }
    assert!(runs >= pts.len());
    invariants_hold(&c, "upload: final").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_crash_at_every_point() {
    run_upload(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_upload_crash_at_every_point() {
    if skip_external() {
        return;
    }
    run_upload(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// CreateChangeset.
// ---------------------------------------------------------------------------------------

struct CreateCase {
    req: v1::CreateChangesetRequest,
    content: Vec<u8>,
}

/// A 1-file change deep in new directories, a delete, and a directory whose tree encoding
/// exceeds the inline threshold (so the create also writes to object storage).
fn create_case(parent: &ObjectId, n: usize, t: &str) -> CreateCase {
    let content = format!("write ordering create {n} {t}\n").into_bytes();
    let path = format!("wo/{n}/deep/er/file.txt");
    let big: Vec<String> = (0..450)
        .map(|i| format!("wo/{n}/big/{i:04}-{}", "p".repeat(135)))
        .collect();
    let mut puts: Vec<(&str, &[u8])> = vec![(&path, &content)];
    puts.extend(big.iter().map(|p| (p.as_str(), content.as_slice())));
    let req = TestCluster::create_request(parent, &puts, &["wo/base.txt"], 1000 + n as u32);
    CreateCase { req, content }
}

async fn create(
    client: &ScionClient,
    req: v1::CreateChangesetRequest,
) -> Result<ObjectId, tonic::Status> {
    let r = client.write().create_changeset(req).await?.into_inner();
    Ok(ObjectId::from_bytes(&r.changeset_id).unwrap())
}

async fn run_create(backend: Backend) {
    let mut c = TestCluster::start(opts(backend)).await;
    let t = tag();
    let alice = c.client(Via::Server, &gh("alice")).await;
    let (root, _) = c.ref_head("main").await;
    // A parent with a file each case deletes (deltas with Put and Delete).
    let base = format!("write ordering base {t}\n").into_bytes();
    let parent = TestCluster::commit(&alice, &root, &[("wo/base.txt", &base)], &[], 999).await;
    let mut n = 0usize;
    let case = create_case(&parent, n, &t);
    TestCluster::upload(&alice, &[&case.content]).await;
    let pts = reached(&c, || async {
        create(&alice, case.req.clone()).await.unwrap();
    })
    .await;
    for want in points::ALL.iter().filter(|p| p.starts_with("create.")) {
        assert!(
            pts.iter().any(|p| p == want),
            "CreateChangeset reaches {want}: {pts:?}"
        );
    }
    assert!(pts.contains(&points::meta_after("commit_changeset")));
    assert!(
        pts.contains(&points::blob_after("put")),
        "the large tree goes to object storage"
    );
    for p in &pts {
        for inj in injections(p) {
            n += 1;
            let what = format!("create {p} {inj:?}");
            let case = create_case(&parent, n, &t);
            let alice = c.client(Via::Server, &gh("alice")).await;
            TestCluster::upload(&alice, &[&case.content]).await;
            let before = c.storage.meta.children(&parent).await.unwrap();
            arm(&c, p, inj);
            let first = create(&alice, case.req.clone()).await;
            recover(&mut c, inj).await;
            let mid = c.storage.meta.children(&parent).await.unwrap();
            invariants_hold(&c, &what).await;
            assert_eager_for_children(&c, &parent, &what).await;

            let alice = c.client(Via::Server, &gh("alice")).await;
            let id = create(&alice, case.req.clone())
                .await
                .unwrap_or_else(|e| panic!("{what}: retry: {e:?}"));
            if let Ok(first) = first {
                assert_eq!(first, id, "{what}: same id");
            }
            // Either the crashed attempt committed this very row, or nothing became visible.
            let new: Vec<&ObjectId> = mid.iter().filter(|x| !before.contains(x)).collect();
            assert!(
                new.is_empty() || new == vec![&id],
                "{what}: unexpected visible changesets {new:?}"
            );
            assert_eq!(
                create(&alice, case.req).await.unwrap(),
                id,
                "{what}: idempotent"
            );
            let after = c.storage.meta.children(&parent).await.unwrap();
            assert_eq!(
                after.iter().filter(|x| **x == id).count(),
                1,
                "{what}: one row"
            );
            assert_eager_for_children(&c, &parent, &what).await;
        }
    }
    invariants_hold(&c, "create: final").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn create_changeset_crash_at_every_point() {
    run_create(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_create_changeset_crash_at_every_point() {
    if skip_external() {
        return;
    }
    run_create(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Land (with a rebase onto a moved tip).
// ---------------------------------------------------------------------------------------

async fn land(client: &ScionClient, head: &ObjectId) -> Result<v1::LandResponse, tonic::Status> {
    client
        .write()
        .land(v1::LandRequest {
            r#ref: "main".into(),
            head: idb(head),
            allow_empty: false,
        })
        .await
        .map(|r| r.into_inner())
}

/// Prepare a land that must rebase: `mine` on the current tip, then another change landed
/// first. Returns `(mine, change number)`.
async fn land_case(c: &TestCluster, n: u32, t: &str) -> (ObjectId, u32) {
    let alice = c.client(Via::Server, &gh("alice")).await;
    let (tip, _) = c.ref_head("main").await;
    let mine_c = format!("land mine {n} {t}\n");
    let theirs_c = format!("land theirs {n} {t}\n");
    let change = 2000 + 2 * n;
    let mine = TestCluster::commit(
        &alice,
        &tip,
        &[(&format!("wl/{n}/mine.txt"), mine_c.as_bytes())],
        &[],
        change,
    )
    .await;
    let theirs = TestCluster::commit(
        &alice,
        &tip,
        &[(&format!("wl/{n}/theirs.txt"), theirs_c.as_bytes())],
        &[],
        change + 1,
    )
    .await;
    land(&alice, &theirs).await.unwrap();
    (mine, change)
}

async fn run_land(backend: Backend) {
    let mut c = TestCluster::start(opts(backend)).await;
    let t = tag();
    let mut n = 0u32;
    let (mine, _) = land_case(&c, n, &t).await;
    let alice = c.client(Via::Server, &gh("alice")).await;
    let pts = reached(&c, || async {
        land(&alice, &mine).await.unwrap();
    })
    .await;
    for want in [
        points::LAND_AFTER_REBASE.to_owned(),
        points::LAND_AFTER_COMMIT.to_owned(),
        points::CREATE_AFTER_COMMIT.to_owned(),
        points::meta_before("cas_ref"),
        points::meta_after("cas_ref"),
    ] {
        assert!(pts.contains(&want), "Land reaches {want}: {pts:?}");
    }
    for p in &pts {
        for inj in injections(p) {
            n += 1;
            let what = format!("land {p} {inj:?}");
            let (mine, change) = land_case(&c, n, &t).await;
            let (tip_before, v_before) = c.ref_head("main").await;
            let alice = c.client(Via::Server, &gh("alice")).await;
            arm(&c, p, inj);
            let first = land(&alice, &mine).await;
            recover(&mut c, inj).await;
            let (tip_mid, v_mid) = c.ref_head("main").await;
            assert!(
                (tip_mid, v_mid) == (tip_before, v_before) || v_mid == v_before + 1,
                "{what}: the ref moved by at most one committed update"
            );
            invariants_hold(&c, &what).await;
            assert_eager_for_children(&c, &tip_before, &what).await;

            let alice = c.client(Via::Server, &gh("alice")).await;
            let r = land(&alice, &mine)
                .await
                .unwrap_or_else(|e| panic!("{what}: retry: {e:?}"));
            let target = ObjectId::from_bytes(&r.target).unwrap();
            if v_mid == v_before + 1 {
                assert_eq!(
                    (target, r.version),
                    (tip_mid, v_mid),
                    "{what}: the recorded result"
                );
            }
            if let Ok(first) = first {
                assert_eq!(
                    (first.target, first.version),
                    (r.target.clone(), r.version),
                    "{what}"
                );
            }
            assert_eq!(
                c.ref_head("main").await,
                (target, r.version),
                "{what}: ref at the result"
            );
            let rec = c
                .storage
                .meta
                .get_landed(
                    &RefName::main(),
                    &ChangeId(TestCluster::change_id(change).try_into().unwrap()),
                )
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("{what}: no landed row"));
            assert_eq!(
                (rec.source, rec.result),
                (mine, target),
                "{what}: landed index"
            );
            assert_eager_for_children(&c, &tip_before, &what).await;
        }
    }
    invariants_hold(&c, "land: final").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn land_crash_at_every_point() {
    run_land(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_land_crash_at_every_point() {
    if skip_external() {
        return;
    }
    run_land(Backend::External).await;
}
