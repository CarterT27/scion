//! SPEC 19.5 Land under concurrency (P32), over a [`TestCluster`] of three servers sharing one
//! storage (multi-process semantics, SPEC 8.4 "no server holds a lock"):
//!
//! - N concurrent clients (N = 10 and 100 by default, N = 1000 in `full_*`) land
//!   non-overlapping changes onto `main` through the `Land` RPC, spread over the servers: all
//!   land, the final tree contains every delta exactly once, and each change id appears
//!   exactly once in `landed` (ref log and `landed` index).
//! - Overlapping text changes that merge cleanly: all land, and the final content equals
//!   `scion_refmodel::land_sequential` applied in the landed order recorded in the ref log.
//! - True conflicts: exactly the conflicting lands fail MERGE_CONFLICT; the others land.
//! - Idempotency (SPEC 16.1 class 5): a crash between the CAS commit and the response
//!   (`FailAfter` at `meta.cas_ref.after`); the retry returns the recorded result and creates
//!   no second ref version.
//! - No locks: the storage `lease_calls` delta is 0 and no `scion::lock` span or event is
//!   logged while the concurrent lands run.
//!
//! Every suite runs on the Memory backend and, as `external_*`, on CockroachDB + S3 when
//! `SCION_TEST_CRDB_URL` and `SCION_TEST_S3_ENDPOINT` are set. Every suite ends with the
//! SPEC 16.2 invariants.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scion_conformance::{Backend, ClusterOpts, TestCluster, invariants};
use scion_core::{ErrorCode, ErrorPayload, FaultAction, ObjectId, Principal, fault::points};
use scion_proto::{ScionClient, error_from_status, v1};
use scion_refmodel::{MDelta, MFile, MTree, land_sequential};
use scion_testkit::encode_tree;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tokio::sync::Barrier;

/// Servers sharing the storage in every suite.
const SERVERS: usize = 3;

fn idb(id: &ObjectId) -> Vec<u8> {
    id.as_bytes().to_vec()
}

fn oid(b: &[u8]) -> ObjectId {
    ObjectId::from_bytes(b).expect("a 34-byte object id")
}

fn alice() -> Principal {
    TestCluster::gh("alice")
}

fn skip_external() -> bool {
    if Backend::external_available() {
        return false;
    }
    println!("SKIP: SCION_TEST_CRDB_URL / SCION_TEST_S3_ENDPOINT unset");
    true
}

fn skip_full() -> bool {
    if std::env::var_os("SCION_FULL").is_some() {
        return false;
    }
    println!("SKIP: SCION_FULL unset");
    true
}

async fn cluster(backend: Backend) -> TestCluster {
    TestCluster::start(
        ClusterOpts::new(backend)
            .with_servers(SERVERS)
            .with_daemon(false),
    )
    .await
}

/// A client of `servers[i]` authenticated as `who`.
async fn connect(c: &TestCluster, i: usize, who: &Principal) -> ScionClient {
    let s = &c.servers[i];
    ScionClient::connect(&s.endpoint, s.tls.clone(), Some(c.token_for(who)))
        .await
        .expect("connect to a cluster server")
}

/// One client per server, all authenticated as `who`.
async fn clients(c: &TestCluster, who: &Principal) -> Vec<ScionClient> {
    let mut out = Vec::with_capacity(c.servers.len());
    for i in 0..c.servers.len() {
        out.push(connect(c, i, who).await);
    }
    out
}

fn land_req(head: &ObjectId) -> v1::LandRequest {
    v1::LandRequest {
        r#ref: "main".into(),
        head: idb(head),
        allow_empty: false,
    }
}

/// `Land` onto `main`, retrying retryable failures (SPEC 7.2: `land_contention` after
/// `land.max_attempts` is UNAVAILABLE and retryable; the ref is unchanged).
async fn land_retrying(
    client: &ScionClient,
    head: &ObjectId,
) -> Result<v1::LandResponse, tonic::Status> {
    let mut tries = 0;
    loop {
        tries += 1;
        match client.write().land(land_req(head)).await {
            Ok(r) => return Ok(r.into_inner()),
            Err(s) if tries < 50 && error_from_status(&s).retryable() => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(s) => return Err(s),
        }
    }
}

/// [`TestCluster::commit`], retrying retryable failures (SPEC 7.2: on the External backend,
/// CockroachDB contention between concurrent creates surfaces as UNAVAILABLE/txn_retry).
async fn commit_retrying(
    client: &ScionClient,
    parent: &ObjectId,
    puts: &[(&str, &[u8])],
    deletes: &[&str],
    change: u32,
) -> ObjectId {
    let contents: Vec<&[u8]> = puts.iter().map(|(_, c)| *c).collect();
    TestCluster::upload(client, &contents).await;
    let req = TestCluster::create_request(parent, puts, deletes, change);
    let mut tries = 0;
    loop {
        tries += 1;
        match client.write().create_changeset(req.clone()).await {
            Ok(r) => return oid(&r.into_inner().changeset_id),
            Err(s) if tries < 50 && error_from_status(&s).retryable() => {
                tokio::time::sleep(std::time::Duration::from_millis(10 * tries)).await;
            }
            Err(s) => panic!("CreateChangeset failed: {s:?}"),
        }
    }
}

/// The whole ref log of `name` (ascending).
async fn ref_log(client: &ScionClient, name: &str) -> Vec<v1::RefLogEntryMsg> {
    let mut out: Vec<v1::RefLogEntryMsg> = Vec::new();
    let mut from = 0;
    loop {
        let page = client
            .refs()
            .get_ref_log(v1::GetRefLogRequest {
                name: name.into(),
                from_version: from,
                limit: 1000,
            })
            .await
            .expect("GetRefLog")
            .into_inner()
            .entries;
        let Some(last) = page.last() else {
            return out;
        };
        from = last.version + 1;
        let n = page.len();
        out.extend(page);
        if n < 1000 {
            return out;
        }
    }
}

/// The tree id of `path` in changeset `cs` (`ListTree`; SPEC 7.4).
async fn tree_id(client: &ScionClient, cs: &ObjectId, path: &str) -> ObjectId {
    let r = client
        .query()
        .list_tree(v1::ListTreeRequest {
            changeset: idb(cs),
            path: path.as_bytes().to_vec(),
        })
        .await
        .expect("ListTree")
        .into_inner();
    oid(&r.tree_id)
}

/// The names in directory `path` of changeset `cs`.
async fn list_names(client: &ScionClient, cs: &ObjectId, path: &str) -> Vec<Vec<u8>> {
    client
        .query()
        .list_tree(v1::ListTreeRequest {
            changeset: idb(cs),
            path: path.as_bytes().to_vec(),
        })
        .await
        .expect("ListTree")
        .into_inner()
        .entries
        .into_iter()
        .map(|e| e.name)
        .collect()
}

/// The verified content of blob `id`.
async fn blob(client: &ScionClient, id: &ObjectId) -> Vec<u8> {
    let f = TestCluster::fetch(client, &[*id], None, None)
        .await
        .expect("GetObjects");
    assert!(f.items[0].verified, "blob {id} must verify");
    f.items[0].content.clone()
}

/// The `(mode, id)` entry of a top-level file of changeset `cs`.
async fn top_file(client: &ScionClient, cs: &ObjectId, name: &str) -> v1::EntryInfo {
    client
        .query()
        .list_tree(v1::ListTreeRequest {
            changeset: idb(cs),
            path: Vec::new(),
        })
        .await
        .expect("ListTree")
        .into_inner()
        .entries
        .into_iter()
        .find(|e| e.name == name.as_bytes())
        .and_then(|e| e.entry)
        .unwrap_or_else(|| panic!("{name} is missing from {cs}"))
}

fn file(content: &[u8]) -> MFile {
    MFile {
        mode: 1,
        content: content.to_vec(),
    }
}

/// Change-id number -> client index for a suite whose change ids are `base + i`.
fn change_index(change_id: &[u8], base: u32) -> usize {
    let n = u32::from_be_bytes(change_id[12..16].try_into().expect("16-byte change id"));
    (n - base) as usize
}

// ---------------------------------------------------------------------------------------
// No locks (SPEC 8.4, 19.5).
// ---------------------------------------------------------------------------------------

/// A window over the storage lease counter and the captured logs.
struct NoLocks {
    leases: u64,
    log_offset: usize,
}

impl NoLocks {
    fn start(c: &TestCluster) -> Self {
        NoLocks {
            leases: c.counters().lease_calls,
            log_offset: c.logs.raw().len(),
        }
    }

    /// No lease call and no `scion::lock` span or event since [`NoLocks::start`].
    fn check(&self, c: &TestCluster, what: &str) {
        let leases = c.counters().lease_calls - self.leases;
        assert_eq!(leases, 0, "{what}: lease calls on the land path");
        let raw = c.logs.raw();
        let window = raw.get(self.log_offset..).unwrap_or("");
        assert!(
            window.contains("land finished"),
            "{what}: the log capture must see the land path (captured nothing)"
        );
        let target = scion_core::trace::LOCK_TARGET;
        let mut lock_lines = Vec::new();
        for line in window.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let in_lock_span = v
                .get("spans")
                .and_then(|s| s.as_array())
                .is_some_and(|spans| {
                    spans
                        .iter()
                        .any(|s| s.get("name").and_then(|n| n.as_str()) == Some("derive_lease"))
                });
            if v.get("target").and_then(|t| t.as_str()) == Some(target) || in_lock_span {
                lock_lines.push(line.to_owned());
            }
        }
        assert!(
            lock_lines.is_empty(),
            "{what}: lock/lease activity logged on the land path: {lock_lines:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// Non-overlapping changes.
// ---------------------------------------------------------------------------------------

async fn run_non_overlapping(backend: Backend, n: usize) {
    let c = cluster(backend).await;
    let clients = clients(&c, &alice()).await;
    let (root, v0) = c.ref_head("main").await;
    let content = |i: usize| format!("non-overlapping change {i}\n").into_bytes();
    let path = |i: usize| format!("lands/{i:05}.txt");

    // Every client creates its change on the root (concurrently).
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..n {
        let client = clients[i % SERVERS].clone();
        tasks.spawn(async move {
            let cs = commit_retrying(
                &client,
                &root,
                &[(&path(i), &content(i))],
                &[],
                1_000 + i as u32,
            )
            .await;
            (i, cs)
        });
    }
    let mut heads = vec![root; n];
    while let Some(r) = tasks.join_next().await {
        let (i, cs) = r.unwrap();
        heads[i] = cs;
    }

    // All clients land at once, spread over the servers.
    let window = NoLocks::start(&c);
    let barrier = Arc::new(Barrier::new(n));
    let mut tasks = tokio::task::JoinSet::new();
    for (i, head) in heads.iter().copied().enumerate() {
        let client = clients[i % SERVERS].clone();
        let barrier = barrier.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            (i, land_retrying(&client, &head).await)
        });
    }
    let mut responses = BTreeMap::new();
    while let Some(r) = tasks.join_next().await {
        let (i, r) = r.unwrap();
        let r = r.unwrap_or_else(|s| panic!("land of client {i} failed: {s:?}"));
        responses.insert(i, r);
    }
    window.check(&c, &format!("{n} concurrent non-overlapping lands"));

    // Every land reports exactly its own change.
    for (i, r) in &responses {
        assert_eq!(r.landed.len(), 1, "client {i} landed entries");
        let l = &r.landed[0];
        assert_eq!(l.change_id, TestCluster::change_id(1_000 + *i as u32));
        assert_eq!(oid(&l.source), heads[*i]);
    }
    let versions: BTreeSet<u64> = responses.values().map(|r| r.version).collect();
    assert_eq!(versions.len(), n, "each land commits its own ref version");

    // Final ref and tree.
    let (tip, v) = c.ref_head("main").await;
    assert_eq!(v, v0 + n as u64, "one ref version per land");
    let expected: MTree = (0..n)
        .map(|i| (path(i).into_bytes(), file(&content(i))))
        .collect();
    let (want_root, _) = encode_tree(&expected);
    assert_eq!(
        tree_id(&clients[0], &tip, "").await,
        want_root,
        "the final tree contains every delta exactly once"
    );
    let names = list_names(&clients[1], &tip, "lands").await;
    assert_eq!(names.len(), n);

    // Each change id appears exactly once in `landed` (ref log and index).
    let log = ref_log(&clients[2], "main").await;
    let mut seen: BTreeMap<Vec<u8>, u32> = BTreeMap::new();
    for e in &log {
        for l in &e.landed {
            *seen.entry(l.change_id.clone()).or_default() += 1;
        }
    }
    for i in 0..n {
        let cid = TestCluster::change_id(1_000 + i as u32);
        assert_eq!(seen.get(&cid), Some(&1), "change {i} in the ref log");
        let row = c
            .storage
            .meta
            .get_landed(
                &scion_object::RefName::main(),
                &scion_core::ChangeId(cid.clone().try_into().unwrap()),
            )
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("change {i} is not in the landed index"));
        assert_eq!(row.source, heads[i]);
        assert_eq!(row.version, responses[&i].version);
    }
    assert_eq!(seen.len(), n, "no other change landed");
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn non_overlapping_lands_n10() {
    run_non_overlapping(Backend::Memory, 10).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn non_overlapping_lands_n100() {
    run_non_overlapping(Backend::Memory, 100).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "spec-sized (SPEC 19.5 N = 1000); run with SCION_FULL=1 --include-ignored"]
async fn full_non_overlapping_lands_n1000() {
    if skip_full() {
        return;
    }
    run_non_overlapping(Backend::Memory, 1000).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn external_non_overlapping_lands() {
    if skip_external() {
        return;
    }
    run_non_overlapping(Backend::External, 10).await;
    run_non_overlapping(Backend::External, 100).await;
}

// ---------------------------------------------------------------------------------------
// Overlapping changes to one file (clean merges and true conflicts).
// ---------------------------------------------------------------------------------------

/// Lines of the shared base file: every client edits its own line `3 i + 1`, so edits of
/// different lines are separated by two unchanged lines (no adjacent hunks).
fn base_lines(slots: usize) -> Vec<Vec<u8>> {
    (0..3 * slots + 1)
        .map(|l| format!("base line {l}\n").into_bytes())
        .collect()
}

fn join(lines: &[Vec<u8>]) -> Vec<u8> {
    lines.concat()
}

/// The file with line `3 slot + 1` replaced by `text`.
fn edited(base: &[Vec<u8>], slot: usize, text: &str) -> Vec<u8> {
    let mut lines = base.to_vec();
    lines[3 * slot + 1] = format!("{text}\n").into_bytes();
    join(&lines)
}

/// Land `base` on `main` and return `(base changeset tip, version after)`.
async fn land_base(c: &TestCluster, client: &ScionClient, content: &[u8]) -> (ObjectId, u64) {
    let (root, _) = c.ref_head("main").await;
    let base = commit_retrying(client, &root, &[("shared.txt", content)], &[], 1).await;
    let r = land_retrying(client, &base).await.expect("land the base");
    (oid(&r.target), r.version)
}

/// Spawn one landing client per `(index, edited content)`; all land at once. Returns each
/// client's result by index.
async fn land_edits(
    clients: &[ScionClient],
    base: &ObjectId,
    edits: Vec<(usize, Vec<u8>)>,
    change_base: u32,
) -> BTreeMap<usize, (ObjectId, Result<v1::LandResponse, tonic::Status>)> {
    let mut heads = Vec::new();
    for (i, content) in &edits {
        let client = &clients[i % SERVERS];
        let cs = commit_retrying(
            client,
            base,
            &[("shared.txt", content)],
            &[],
            change_base + *i as u32,
        )
        .await;
        heads.push((*i, cs));
    }
    let barrier = Arc::new(Barrier::new(heads.len()));
    let mut tasks = tokio::task::JoinSet::new();
    for (i, head) in heads {
        let client = clients[i % SERVERS].clone();
        let barrier = barrier.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            (i, head, land_retrying(&client, &head).await)
        });
    }
    let mut out = BTreeMap::new();
    while let Some(r) = tasks.join_next().await {
        let (i, head, r) = r.unwrap();
        out.insert(i, (head, r));
    }
    out
}

/// The client indices in the order their changes landed after `after_version` (ref log).
async fn landed_order(client: &ScionClient, after_version: u64, change_base: u32) -> Vec<usize> {
    ref_log(client, "main")
        .await
        .into_iter()
        .filter(|e| e.version > after_version)
        .flat_map(|e| e.landed)
        .map(|l| change_index(&l.change_id, change_base))
        .collect()
}

async fn run_overlapping_clean(backend: Backend, n: usize) {
    let c = cluster(backend).await;
    let clients = clients(&c, &alice()).await;
    let base = base_lines(n);
    let (tip0, v0) = land_base(&c, &clients[0], &join(&base)).await;
    let change_base = 5_000;
    let edits: Vec<(usize, Vec<u8>)> = (0..n)
        .map(|i| (i, edited(&base, i, &format!("client {i} was here"))))
        .collect();
    let window = NoLocks::start(&c);
    let results = land_edits(&clients, &tip0, edits.clone(), change_base).await;
    window.check(&c, "concurrent overlapping lands");
    for (i, (_, r)) in &results {
        assert!(r.is_ok(), "client {i}: a clean merge must land ({r:?})");
    }

    let order = landed_order(&clients[1], v0, change_base).await;
    assert_eq!(order.len(), n, "every change landed once");
    assert_eq!(
        order.iter().copied().collect::<BTreeSet<_>>().len(),
        n,
        "no change landed twice"
    );
    // The reference model's sequential application in the landed order.
    let base_tree: MTree = [(b"shared.txt".to_vec(), file(&join(&base)))].into();
    let changes: Vec<Vec<MDelta>> = order
        .iter()
        .map(|&i| {
            vec![MDelta::Put(
                b"shared.txt".to_vec(),
                file(&edits[i].1.clone()),
            )]
        })
        .collect();
    let model = land_sequential(&base_tree, &changes).expect("the model lands every change");
    let (tip, v) = c.ref_head("main").await;
    assert_eq!(v, v0 + n as u64);
    let entry = top_file(&clients[2], &tip, "shared.txt").await;
    let content = blob(&clients[2], &oid(&entry.id)).await;
    assert_eq!(
        content,
        model[b"shared.txt".as_slice()].content,
        "final content equals the model's sequential application"
    );
    let mut all = base.clone();
    for i in 0..n {
        all[3 * i + 1] = format!("client {i} was here\n").into_bytes();
    }
    assert_eq!(content, join(&all), "every edit appears exactly once");
    assert_eq!(tree_id(&clients[0], &tip, "").await, encode_tree(&model).0);
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn overlapping_clean_merges_all_land() {
    run_overlapping_clean(Backend::Memory, 10).await;
    run_overlapping_clean(Backend::Memory, 40).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "spec-sized; run with SCION_FULL=1 --include-ignored"]
async fn full_overlapping_clean_merges_all_land() {
    if skip_full() {
        return;
    }
    run_overlapping_clean(Backend::Memory, 300).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn external_overlapping_clean_merges_all_land() {
    if skip_external() {
        return;
    }
    run_overlapping_clean(Backend::External, 10).await;
}

/// `pairs` pairs of clients edit the same line with different text (true conflicts); `solo`
/// further clients edit lines of their own.
async fn run_true_conflicts(backend: Backend, pairs: usize, solo: usize) {
    let c = cluster(backend).await;
    let clients = clients(&c, &alice()).await;
    let slots = pairs + solo;
    let base = base_lines(slots);
    let (tip0, v0) = land_base(&c, &clients[0], &join(&base)).await;
    let change_base = 7_000;
    // Clients 0..2*pairs form pairs (2p, 2p+1) on slot p; the rest edit slots pairs.. alone.
    let mut edits: Vec<(usize, Vec<u8>)> = Vec::new();
    for p in 0..pairs {
        edits.push((2 * p, edited(&base, p, &format!("pair {p} side A"))));
        edits.push((2 * p + 1, edited(&base, p, &format!("pair {p} side B"))));
    }
    for s in 0..solo {
        let i = 2 * pairs + s;
        edits.push((i, edited(&base, pairs + s, &format!("solo {s}"))));
    }
    let results = land_edits(&clients, &tip0, edits.clone(), change_base).await;

    let mut failed = BTreeSet::new();
    for (i, (_, r)) in &results {
        match r {
            Ok(_) => {}
            Err(s) => {
                let e = error_from_status(s);
                assert_eq!(e.code, ErrorCode::MergeConflict, "client {i}: {s:?}");
                let ErrorPayload::MergeConflict(report) = &e.payload else {
                    panic!("client {i}: MERGE_CONFLICT without a report");
                };
                let paths: Vec<&[u8]> =
                    report.conflicts.iter().map(|x| x.path.as_slice()).collect();
                assert_eq!(paths, vec![b"shared.txt".as_slice()], "client {i}");
                assert_eq!(report.conflicts[0].kind.as_str(), "content");
                failed.insert(*i);
            }
        }
    }
    // Exactly one side of every pair fails; every solo client lands.
    for p in 0..pairs {
        let a = failed.contains(&(2 * p));
        let b = failed.contains(&(2 * p + 1));
        assert!(
            a != b,
            "pair {p}: exactly one side must fail (failed: {failed:?})"
        );
    }
    for s in 0..solo {
        assert!(
            !failed.contains(&(2 * pairs + s)),
            "solo client {s} must land"
        );
    }
    assert_eq!(failed.len(), pairs);

    // The landed order replayed by the model gives the final content, and each failed change
    // conflicts in the model too.
    let order = landed_order(&clients[1], v0, change_base).await;
    assert_eq!(order.len(), pairs + solo);
    let base_tree: MTree = [(b"shared.txt".to_vec(), file(&join(&base)))].into();
    let delta = |i: usize| vec![MDelta::Put(b"shared.txt".to_vec(), file(&edits[i].1))];
    let changes: Vec<Vec<MDelta>> = order.iter().map(|&i| delta(i)).collect();
    let model = land_sequential(&base_tree, &changes).expect("the landed order is clean");
    let (tip, v) = c.ref_head("main").await;
    assert_eq!(
        v,
        v0 + (pairs + solo) as u64,
        "failed lands leave no version"
    );
    let entry = top_file(&clients[2], &tip, "shared.txt").await;
    assert_eq!(
        blob(&clients[2], &oid(&entry.id)).await,
        model[b"shared.txt".as_slice()].content
    );
    for &f in &failed {
        let mut with_failed = changes.clone();
        with_failed.push(delta(f));
        let (at, conflicts) = land_sequential(&base_tree, &with_failed)
            .expect_err("the failed change conflicts in the model");
        assert_eq!(at, with_failed.len() - 1);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, "content");
    }
    // The failed changes are not in the landed index.
    for &f in &failed {
        let cid = TestCluster::change_id(change_base + f as u32);
        let row = c
            .storage
            .meta
            .get_landed(
                &scion_object::RefName::main(),
                &scion_core::ChangeId(cid.try_into().unwrap()),
            )
            .await
            .unwrap();
        assert!(row.is_none(), "failed change {f} must not be landed");
    }
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn true_conflicts_fail_exactly_the_conflicting_lands() {
    run_true_conflicts(Backend::Memory, 3, 4).await;
    run_true_conflicts(Backend::Memory, 15, 20).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn external_true_conflicts_fail_exactly_the_conflicting_lands() {
    if skip_external() {
        return;
    }
    run_true_conflicts(Backend::External, 3, 4).await;
}

// ---------------------------------------------------------------------------------------
// Idempotency after a crash between CAS commit and response (SPEC 16.1 class 5).
// ---------------------------------------------------------------------------------------

async fn run_idempotent_retry(backend: Backend) {
    let c = cluster(backend).await;
    let clients = clients(&c, &alice()).await;
    let (root, _) = c.ref_head("main").await;
    // Move `main` first so that the stack below is rebased (its results differ from its
    // sources and the retry must find them in the `landed` index).
    let pre = commit_retrying(&clients[0], &root, &[("idem/pre.txt", b"p\n")], &[], 41).await;
    land_retrying(&clients[0], &pre)
        .await
        .expect("land the first change");
    let (_, v0) = c.ref_head("main").await;
    let head = commit_retrying(&clients[0], &root, &[("idem/a.txt", b"a\n")], &[], 42).await;
    // A two-member stack: both members are recorded by the one committed CAS.
    let top = commit_retrying(&clients[0], &head, &[("idem/b.txt", b"b\n")], &[], 43).await;

    let faults = c.faults();
    let point = points::meta_after("cas_ref");
    faults.arm(&point, FaultAction::FailAfter, Some(1));
    let s = clients[0]
        .write()
        .land(land_req(&top))
        .await
        .expect_err("the response is lost after the commit");
    faults.disarm_all();
    let e = error_from_status(&s);
    assert_eq!(e.code, ErrorCode::Unavailable, "{s:?}");
    assert!(e.retryable());

    // The CAS committed exactly once.
    let (tip, v1) = c.ref_head("main").await;
    assert_eq!(v1, v0 + 1, "the commit happened before the crash");
    let log_before = ref_log(&clients[1], "main").await;

    // The retry, on another server, returns the recorded result.
    for client in &clients[1..] {
        let r = client
            .write()
            .land(land_req(&top))
            .await
            .expect("the retry succeeds")
            .into_inner();
        assert_eq!(oid(&r.target), tip, "recorded target");
        assert_eq!(r.version, v1, "recorded version, no second version");
        let mut cids: Vec<Vec<u8>> = r.landed.iter().map(|l| l.change_id.clone()).collect();
        cids.sort();
        assert_eq!(
            cids,
            vec![TestCluster::change_id(42), TestCluster::change_id(43)]
        );
        let sources: BTreeSet<ObjectId> = r.landed.iter().map(|l| oid(&l.source)).collect();
        assert_eq!(sources, [head, top].into());
        let last = r.landed.iter().find(|l| oid(&l.source) == top).unwrap();
        assert_eq!(oid(&last.result), tip);
        assert_ne!(tip, top, "the stack was rebased");
    }
    let (tip2, v2) = c.ref_head("main").await;
    assert_eq!(
        (tip2, v2),
        (tip, v1),
        "retries create no second ref version"
    );
    let log_after = ref_log(&clients[2], "main").await;
    assert_eq!(log_after.len(), log_before.len());
    assert_eq!(log_after.last().unwrap().version, v1);

    // A lost response on a concurrent workload: every client retries until it succeeds, and
    // each change lands exactly once.
    let n = 12;
    let mut heads = Vec::new();
    for i in 0..n {
        let p = format!("idem/c{i}.txt");
        heads.push(
            commit_retrying(
                &clients[i % SERVERS],
                &root,
                &[(&p, format!("c{i}\n").as_bytes())],
                &[],
                100 + i as u32,
            )
            .await,
        );
    }
    faults.arm(&point, FaultAction::FailAfter, Some(4));
    let lost = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut tasks = tokio::task::JoinSet::new();
    for (i, h) in heads.iter().copied().enumerate() {
        let client = clients[i % SERVERS].clone();
        let lost = lost.clone();
        tasks.spawn(async move {
            // Retry like `land_retrying`, counting the injected lost responses.
            for _ in 0..50 {
                match client.write().land(land_req(&h)).await {
                    Ok(r) => return (i, Ok(r.into_inner())),
                    Err(s) if error_from_status(&s).retryable() => {
                        if error_from_status(&s).subcode() == Some(scion_core::subcode::INJECTED) {
                            lost.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    Err(s) => return (i, Err(s)),
                }
            }
            (i, land_retrying(&client, &h).await)
        });
    }
    let mut reported: BTreeMap<Vec<u8>, (Vec<u8>, Vec<u8>)> = BTreeMap::new();
    while let Some(r) = tasks.join_next().await {
        let (i, r) = r.unwrap();
        let r = r.unwrap_or_else(|s| panic!("client {i} never landed: {s:?}"));
        assert_eq!(r.landed.len(), 1, "client {i} landed entries");
        let l = &r.landed[0];
        assert_eq!(l.change_id, TestCluster::change_id(100 + i as u32));
        reported.insert(l.change_id.clone(), (l.source.clone(), l.result.clone()));
    }
    faults.disarm_all();
    assert!(
        lost.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "no response was lost: the FailAfter fault never fired on the land path"
    );

    let (_, v3) = c.ref_head("main").await;
    assert_eq!(
        v3,
        v1 + n as u64,
        "one version per change despite lost responses"
    );
    let log = ref_log(&clients[0], "main").await;
    let mut count: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    for e in &log {
        for l in &e.landed {
            *count.entry(l.change_id.clone()).or_default() += 1;
        }
    }
    for i in 0..n {
        assert_eq!(count.get(&TestCluster::change_id(100 + i as u32)), Some(&1));
    }
    // Retried or not, every client got the landed entry recorded in the ref log (SPEC 18.4:
    // an idempotent retry returns the recorded `landed` rows with the current target and
    // version, so only the entries are compared here).
    let logged: BTreeMap<Vec<u8>, (Vec<u8>, Vec<u8>)> = log
        .iter()
        .flat_map(|e| e.landed.iter())
        .filter(|l| reported.contains_key(&l.change_id))
        .map(|l| (l.change_id.clone(), (l.source.clone(), l.result.clone())))
        .collect();
    assert_eq!(
        reported, logged,
        "responses carry the recorded landed entries"
    );
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn crash_after_cas_commit_retry_returns_recorded_result() {
    run_idempotent_retry(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn external_crash_after_cas_commit_retry_returns_recorded_result() {
    if skip_external() {
        return;
    }
    run_idempotent_retry(Backend::External).await;
}
