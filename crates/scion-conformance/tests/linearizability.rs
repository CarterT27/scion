//! SPEC 19.5 linearizability of ref operations (P32), with the SPEC 19.0 History Recorder.
//!
//! Concurrent `ResolveRef` / `UpdateRef` / `Land` calls from several clients, spread over a
//! [`TestCluster`] of three servers sharing one storage, are recorded with
//! [`scion_testkit::lincheck::HistoryRecorder`] and checked with
//! [`scion_testkit::lincheck::check_ref_registers`] (WGL search over versioned CAS registers),
//! about 200 operations per history and one fresh ref per seed, over many seeds.
//!
//! Faults injected while the clients run:
//!
//! - **Server kills**: `kill_server` / `restart_server` of a random server mid-run. Calls that
//!   fail without a definite answer (transport errors, `UNAVAILABLE` other than
//!   `land_contention`) complete as `Unknown` (may or may not have taken effect); clients
//!   reconnect after a restart.
//! - **Delays**: the storage fault registry's `Delay` at metadata points (`meta.get_ref.before`
//!   and `meta.cas_ref.before|after`), which stretches the window between a commit and its
//!   response and between the read and the CAS of an update or land.
//! - **Lost responses**: `FailAfter` at `meta.cas_ref.after` (the commit happens, the response
//!   is UNAVAILABLE: an `Unknown` completion).
//!
//! The checker must report zero violations. A search that exhausts its step budget is
//! inconclusive rather than a violation; such a seed is recorded again on a fresh ref (at
//! most twice), and a definite violation fails at once. Each history also passes a negative
//! control: the same events with the final read claiming an unreachable version must be
//! rejected. Every suite runs on the Memory backend and, as
//! `external_*`, on CockroachDB + S3 when `SCION_TEST_CRDB_URL` and `SCION_TEST_S3_ENDPOINT`
//! are set; spec-sized seed counts are in `full_*` (SCION_FULL).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use scion_conformance::{Backend, ClusterOpts, TestCluster, invariants};
use scion_core::{
    ErrorCode, ErrorPayload, FaultAction, ObjectId, Principal, fault::points, subcode,
};
use scion_proto::{Endpoint, ScionClient, TlsOptions, error_from_status, v1};
use scion_testkit::lincheck::{
    HistoryRecorder, RefOp, RefRet, check_ref_registers, check_ref_registers_with_initial,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// Servers sharing the storage.
const SERVERS: usize = 3;
/// Concurrent clients per history (client `i` talks to server `i % SERVERS`).
const CLIENTS: usize = 6;
/// Operations per history (PLAN 3.4: about 200).
const OPS: usize = 200;
/// Pre-created changesets every history may land (each at most once per ref).
const HEADS: usize = 120;
/// Pre-created targets for `UpdateRef`.
const POOL: usize = 8;

fn idb(id: &ObjectId) -> Vec<u8> {
    id.as_bytes().to_vec()
}

fn raw(b: &[u8]) -> [u8; 34] {
    b.try_into().expect("a 34-byte object id")
}

/// Every client writes as `github:admin`: `update` and `force` on every ref, write on `/`.
fn admin() -> Principal {
    TestCluster::gh("admin")
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

/// Which faults a run injects, and the size of its histories.
#[derive(Clone, Copy, Debug)]
struct Faults {
    kills: bool,
    delays: bool,
    lost_responses: bool,
    /// Pause between fault actions, in milliseconds (`lo..hi`).
    every_ms: (u64, u64),
    /// Concurrent clients.
    clients: usize,
    /// Operations per history.
    ops: usize,
}

const NO_FAULTS: Faults = Faults {
    kills: false,
    delays: false,
    lost_responses: false,
    every_ms: (5, 25),
    clients: CLIENTS,
    ops: OPS,
};

const ALL_FAULTS: Faults = Faults {
    kills: true,
    delays: true,
    lost_responses: true,
    every_ms: (5, 25),
    clients: CLIENTS,
    ops: OPS,
};

/// A lazily (re)connecting client of one server.
struct Conn {
    endpoint: Endpoint,
    tls: Option<TlsOptions>,
    token: String,
    client: Option<ScionClient>,
}

impl Conn {
    async fn get(&mut self) -> Option<ScionClient> {
        if self.client.is_none() {
            self.client =
                ScionClient::connect(&self.endpoint, self.tls.clone(), Some(self.token.clone()))
                    .await
                    .ok();
        }
        self.client.clone()
    }

    /// Forget the connection after a transport-level failure (the server may have been
    /// killed and restarted).
    fn reset(&mut self) {
        self.client = None;
    }
}

/// True when the status carries a Scion error (an answer from a live server), false for
/// transport failures (killed server, broken connection).
fn is_scion_answer(s: &tonic::Status) -> bool {
    TestCluster::error_detail(s).is_some()
}

/// The fixed material of one cluster: land heads and update targets on the root.
struct Fixture {
    root: ObjectId,
    heads: Vec<ObjectId>,
    pool: Vec<ObjectId>,
}

async fn fixture(c: &TestCluster) -> Fixture {
    let client = c.client(scion_conformance::Via::Server, &admin()).await;
    let (root, _) = c.ref_head("main").await;
    let mut heads = Vec::with_capacity(HEADS);
    for j in 0..HEADS {
        let p = format!("lin/land/{j:04}.txt");
        let content = format!("land head {j}\n");
        heads.push(
            TestCluster::commit(&client, &root, &[(&p, content.as_bytes())], &[], j as u32).await,
        );
    }
    let mut pool = Vec::with_capacity(POOL);
    for k in 0..POOL {
        let p = format!("lin/pool/{k}.txt");
        let content = format!("pool target {k}\n");
        pool.push(
            TestCluster::commit(
                &client,
                &root,
                &[(&p, content.as_bytes())],
                &[],
                10_000 + k as u32,
            )
            .await,
        );
    }
    Fixture { root, heads, pool }
}

/// State shared by the clients of one history.
struct Shared {
    rec: HistoryRecorder,
    ref_name: String,
    /// Operations issued so far.
    issued: AtomicUsize,
    /// Operations to issue.
    ops: usize,
    /// Next unused land head.
    next_head: AtomicUsize,
    heads: Vec<ObjectId>,
    pool: Vec<ObjectId>,
}

/// One client: issues operations until the history has about `Shared::ops` of them.
async fn client_loop(id: u32, mut conn: Conn, sh: Arc<Shared>, seed: u64) {
    let mut rng = ChaCha8Rng::seed_from_u64(seed ^ (u64::from(id) << 32) ^ 0x5eed);
    let mut seen_version: u64 = 1;
    while sh.issued.load(Ordering::SeqCst) < sh.ops {
        let Some(client) = conn.get().await else {
            // The server is down: the call could not even be sent (no invocation recorded).
            tokio::time::sleep(Duration::from_millis(5)).await;
            continue;
        };
        // Think time, so that faults land among in-flight calls.
        tokio::time::sleep(Duration::from_millis(rng.random_range(0..6))).await;
        let roll = rng.random_range(0..100);
        if roll < 40 {
            sh.issued.fetch_add(1, Ordering::SeqCst);
            let t = sh.rec.invoke(id, &sh.ref_name, RefOp::Read);
            let r = client
                .objects()
                .resolve_ref(v1::ResolveRefRequest {
                    name: sh.ref_name.clone(),
                    at_version: None,
                })
                .await;
            match r {
                Ok(r) => {
                    let r = r.into_inner();
                    seen_version = r.version;
                    sh.rec.complete(
                        t,
                        RefRet::Value {
                            version: r.version,
                            target: raw(&r.target),
                        },
                    );
                }
                Err(s) => {
                    sh.rec.complete(t, RefRet::Unknown);
                    if !is_scion_answer(&s) {
                        conn.reset();
                    }
                }
            }
        } else if roll < 70 {
            let expect = match rng.random_range(0..10) {
                0 => seen_version.saturating_sub(1),
                1 => seen_version + 1,
                _ => seen_version,
            };
            let target = sh.pool[rng.random_range(0..sh.pool.len())];
            sh.issued.fetch_add(1, Ordering::SeqCst);
            let t = sh.rec.invoke(
                id,
                &sh.ref_name,
                RefOp::Cas {
                    expect,
                    target: *target.as_bytes(),
                },
            );
            let r = client
                .refs()
                .update_ref(v1::UpdateRefRequest {
                    name: sh.ref_name.clone(),
                    expected_version: expect,
                    new_target: idb(&target),
                    allow_non_fast_forward: true,
                })
                .await;
            match r {
                Ok(r) => {
                    let version = r.into_inner().version;
                    seen_version = version;
                    sh.rec.complete(
                        t,
                        RefRet::Ok {
                            version,
                            target: *target.as_bytes(),
                        },
                    );
                }
                Err(s) => {
                    let e = error_from_status(&s);
                    let ret = match e.code {
                        _ if !is_scion_answer(&s) => RefRet::Unknown,
                        ErrorCode::RefVersionMismatch => {
                            if let ErrorPayload::RefMismatch {
                                current_version, ..
                            } = e.payload
                            {
                                seen_version = current_version;
                            }
                            RefRet::Mismatch
                        }
                        // An exhausted storage transaction retry committed nothing.
                        ErrorCode::Unavailable if e.subcode() == Some(subcode::TXN_RETRY) => {
                            RefRet::Fail
                        }
                        ErrorCode::NotFastForward
                        | ErrorCode::PermissionDenied
                        | ErrorCode::InvalidArgument
                        | ErrorCode::NotFound => RefRet::Fail,
                        _ => RefRet::Unknown,
                    };
                    if !is_scion_answer(&s) {
                        conn.reset();
                    }
                    sh.rec.complete(t, ret);
                }
            }
        } else {
            let j = sh.next_head.fetch_add(1, Ordering::SeqCst);
            let Some(head) = sh.heads.get(j).copied() else {
                continue;
            };
            let mut change = [0u8; 16];
            change.copy_from_slice(&TestCluster::change_id(j as u32));
            sh.issued.fetch_add(1, Ordering::SeqCst);
            let t = sh.rec.invoke(id, &sh.ref_name, RefOp::Land { change });
            let r = client
                .write()
                .land(v1::LandRequest {
                    r#ref: sh.ref_name.clone(),
                    head: idb(&head),
                    allow_empty: false,
                })
                .await;
            match r {
                Ok(r) => {
                    let r = r.into_inner();
                    seen_version = r.version;
                    sh.rec.complete(
                        t,
                        RefRet::Ok {
                            version: r.version,
                            target: raw(&r.target),
                        },
                    );
                }
                Err(s) => {
                    let e = error_from_status(&s);
                    let ret = match e.code {
                        _ if !is_scion_answer(&s) => RefRet::Unknown,
                        // The ref is unchanged after max_attempts (SPEC 18.4), and an
                        // exhausted storage transaction retry committed nothing.
                        ErrorCode::Unavailable
                            if matches!(
                                e.subcode(),
                                Some(subcode::LAND_CONTENTION | subcode::TXN_RETRY)
                            ) =>
                        {
                            RefRet::Fail
                        }
                        ErrorCode::MergeConflict
                        | ErrorCode::EmptyChange
                        | ErrorCode::AlreadyLanded
                        | ErrorCode::PermissionDenied
                        | ErrorCode::InvalidArgument
                        | ErrorCode::NotFound => RefRet::Fail,
                        _ => RefRet::Unknown,
                    };
                    if !is_scion_answer(&s) {
                        conn.reset();
                    }
                    sh.rec.complete(t, ret);
                }
            }
        }
    }
}

/// The fault driver: until every client finished, kill and restart servers, arm delays and
/// lost responses at random.
async fn drive_faults(
    c: &mut TestCluster,
    faults: Faults,
    rng: &mut ChaCha8Rng,
    done: &AtomicBool,
) -> u64 {
    let mut kills = 0;
    let reg = c.faults();
    let delay_points = [
        points::meta_before("get_ref"),
        points::meta_before("cas_ref"),
        points::meta_after("cas_ref"),
    ];
    while !done.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(
            rng.random_range(faults.every_ms.0..faults.every_ms.1),
        ))
        .await;
        match rng.random_range(0..3) {
            0 if faults.kills => {
                let i = rng.random_range(0..c.servers.len());
                c.kill_server(i).await;
                tokio::time::sleep(Duration::from_millis(rng.random_range(5..30))).await;
                // Startup runs the bootstrap through the same storage; an armed fault there
                // would fail the restart itself (not a client operation).
                reg.disarm_all();
                c.restart_server(i).await;
                kills += 1;
            }
            1 if faults.delays => {
                let p = &delay_points[rng.random_range(0..delay_points.len())];
                let d = Duration::from_millis(rng.random_range(1..15));
                reg.arm(p, FaultAction::Delay(d), Some(rng.random_range(1..8)));
            }
            2 if faults.lost_responses => {
                reg.arm(
                    &points::meta_after("cas_ref"),
                    FaultAction::FailAfter,
                    Some(1),
                );
            }
            _ => {}
        }
    }
    reg.disarm_all();
    kills
}

/// Record and check one history on a fresh ref. A definite violation panics; an
/// inconclusive search (budget exhausted, see `scion_testkit::lincheck`) is returned as
/// `Err` so the caller can record a fresh history for the seed.
async fn run_seed(
    c: &mut TestCluster,
    fx: &Fixture,
    seed: u64,
    attempt: u32,
    faults: Faults,
) -> Result<Stats, String> {
    let ref_name = format!("lin/s{seed}-{attempt}");
    let admin_client = c.client(scion_conformance::Via::Server, &admin()).await;
    let created = admin_client
        .refs()
        .create_ref(v1::CreateRefRequest {
            name: ref_name.clone(),
            target: idb(&fx.root),
            reason: "linearizability".into(),
        })
        .await
        .expect("CreateRef")
        .into_inner();
    assert_eq!(created.version, 1);

    let sh = Arc::new(Shared {
        rec: HistoryRecorder::new(),
        ref_name: ref_name.clone(),
        issued: AtomicUsize::new(0),
        ops: faults.ops,
        next_head: AtomicUsize::new(0),
        heads: fx.heads.clone(),
        pool: fx.pool.clone(),
    });
    let token = c.token_for(&admin());
    let mut tasks = tokio::task::JoinSet::new();
    for id in 0..faults.clients as u32 {
        let s = &c.servers[id as usize % c.servers.len()];
        let conn = Conn {
            endpoint: s.endpoint.clone(),
            tls: s.tls.clone(),
            token: token.clone(),
            client: None,
        };
        tasks.spawn(client_loop(id, conn, sh.clone(), seed));
    }
    let done = Arc::new(AtomicBool::new(false));
    let waiter = {
        let done = done.clone();
        tokio::spawn(async move {
            while let Some(r) = tasks.join_next().await {
                r.expect("client task");
            }
            done.store(true, Ordering::SeqCst);
        })
    };
    let mut rng = ChaCha8Rng::seed_from_u64(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let kills = drive_faults(c, faults, &mut rng, &done).await;
    waiter.await.unwrap();

    // A final read after quiescence, so the recorded history pins the final state too.
    let final_client = c.client(scion_conformance::Via::Server, &admin()).await;
    let t = sh.rec.invoke(faults.clients as u32, &ref_name, RefOp::Read);
    let r = final_client
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: ref_name.clone(),
            at_version: None,
        })
        .await
        .expect("final ResolveRef")
        .into_inner();
    sh.rec.complete(
        t,
        RefRet::Value {
            version: r.version,
            target: raw(&r.target),
        },
    );

    let events = sh.rec.events();
    let completed = events.iter().filter(|e| e.ret.is_some()).count();
    let invoked = events.iter().filter(|e| e.call.is_some()).count();
    assert_eq!(
        completed, invoked,
        "seed {seed}: every invocation completed"
    );
    assert!(
        invoked >= faults.ops / 2,
        "seed {seed}: only {invoked} operations were issued"
    );
    if let Err(v) = check_ref_registers(&events) {
        if v.detail.contains("inconclusive") {
            return Err(format!("seed {seed} attempt {attempt}: {v}"));
        }
        panic!("seed {seed} ({faults:?}): linearizability violation: {v}\nhistory: {events:#?}");
    }
    let initial: BTreeMap<String, (u64, [u8; 34])> =
        [(ref_name.clone(), (1, *fx.root.as_bytes()))].into();
    if let Err(v) = check_ref_registers_with_initial(&events, &initial) {
        if v.detail.contains("inconclusive") {
            return Err(format!("seed {seed} attempt {attempt}: {v}"));
        }
        panic!("seed {seed} ({faults:?}): violation from the known initial state: {v}");
    }

    // Negative control: the same history with the final read claiming a version above
    // anything the effectful operations (successful or unknown) could have reached must be
    // rejected, so the checker really constrains these histories.
    let stats = Stats::of(&events, kills);
    let reachable = 1 + stats.cas_ok + stats.lands_ok + stats.unknown;
    let mut tampered = events.clone();
    if let Some(RefRet::Value { version, .. }) = tampered.last_mut().and_then(|e| e.ret.as_mut()) {
        *version = reachable + 1;
    }
    assert!(
        check_ref_registers_with_initial(&tampered, &initial).is_err(),
        "seed {seed}: a tampered history must not be linearizable"
    );

    // Second negative control (real-time order): one more read, invoked after the final read
    // completed, that observes the version before the final one (a stale read). Versions
    // only grow, so this must be a definite violation, not merely an exhausted search.
    if r.version > 1 {
        let mut stale = events.clone();
        let op = stale.iter().map(|e| e.op).max().unwrap_or(0) + 1;
        let at = stale.iter().map(|e| e.at).max().unwrap_or(0);
        let client = faults.clients as u32 + 1;
        stale.push(scion_testkit::lincheck::Event {
            op,
            client,
            r: ref_name.clone(),
            call: Some(RefOp::Read),
            ret: None,
            at: at + 1,
        });
        stale.push(scion_testkit::lincheck::Event {
            op,
            client,
            r: ref_name.clone(),
            call: None,
            ret: Some(RefRet::Value {
                version: r.version - 1,
                target: raw(&r.target),
            }),
            at: at + 2,
        });
        match check_ref_registers_with_initial(&stale, &initial) {
            Ok(()) => panic!("seed {seed}: a stale read after the final read was accepted"),
            Err(v) => assert!(
                !v.detail.contains("inconclusive"),
                "seed {seed}: the stale-read control must be a definite violation: {v}"
            ),
        }
    }
    Ok(stats)
}

/// Outcome counts of recorded histories (to show the runs exercise every outcome).
#[derive(Debug, Default)]
struct Stats {
    reads: u64,
    cas_ok: u64,
    mismatches: u64,
    lands_ok: u64,
    fails: u64,
    unknown: u64,
    kills: u64,
}

impl Stats {
    fn of(events: &[scion_testkit::lincheck::Event], kills: u64) -> Self {
        let calls: BTreeMap<u64, &RefOp> = events
            .iter()
            .filter_map(|e| e.call.as_ref().map(|c| (e.op, c)))
            .collect();
        let mut s = Stats {
            kills,
            ..Stats::default()
        };
        for e in events {
            let Some(ret) = &e.ret else { continue };
            match (calls.get(&e.op), ret) {
                (_, RefRet::Unknown) => s.unknown += 1,
                (_, RefRet::Fail) => s.fails += 1,
                (_, RefRet::Mismatch) => s.mismatches += 1,
                (_, RefRet::Value { .. }) => s.reads += 1,
                (Some(RefOp::Land { .. }), RefRet::Ok { .. }) => s.lands_ok += 1,
                (_, RefRet::Ok { .. }) => s.cas_ok += 1,
            }
        }
        s
    }

    fn add(&mut self, o: &Stats) {
        self.reads += o.reads;
        self.cas_ok += o.cas_ok;
        self.mismatches += o.mismatches;
        self.lands_ok += o.lands_ok;
        self.fails += o.fails;
        self.unknown += o.unknown;
        self.kills += o.kills;
    }
}

async fn run(backend: Backend, seeds: std::ops::Range<u64>, faults: Faults) {
    let mut c = TestCluster::start(
        ClusterOpts::new(backend)
            .with_servers(SERVERS)
            .with_daemon(false),
    )
    .await;
    let fx = fixture(&c).await;
    let mut total = Stats::default();
    let mut inconclusive = Vec::new();
    for seed in seeds {
        let mut attempt = 0;
        loop {
            match run_seed(&mut c, &fx, seed, attempt, faults).await {
                Ok(stats) => {
                    total.add(&stats);
                    break;
                }
                Err(e) if attempt < 2 => {
                    inconclusive.push(e);
                    attempt += 1;
                }
                Err(e) => panic!("seed {seed}: three inconclusive histories in a row: {e}"),
            }
        }
    }
    if !inconclusive.is_empty() {
        println!("re-recorded after an inconclusive search: {inconclusive:?}");
    }
    println!("linearizability ({faults:?}): {total:?}");
    assert!(total.reads > 0 && total.cas_ok > 0 && total.lands_ok > 0 && total.mismatches > 0);
    if faults.kills {
        assert!(total.kills > 0, "no server was killed");
    }
    if faults.kills || faults.lost_responses {
        assert!(total.unknown > 0, "no operation completed as Unknown");
    }
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ref_histories_are_linearizable_without_faults() {
    run(Backend::Memory, 0..4, NO_FAULTS).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ref_histories_are_linearizable_with_kills_and_delays() {
    run(Backend::Memory, 100..112, ALL_FAULTS).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ref_histories_are_linearizable_with_kills_only() {
    run(
        Backend::Memory,
        200..206,
        Faults {
            kills: true,
            delays: false,
            lost_responses: false,
            every_ms: (5, 25),
            clients: CLIENTS,
            ops: OPS,
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "spec-sized seed count; run with SCION_FULL=1 --include-ignored"]
async fn full_ref_histories_are_linearizable_with_kills_and_delays() {
    if skip_full() {
        return;
    }
    run(Backend::Memory, 1_000..1_200, ALL_FAULTS).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn external_ref_histories_are_linearizable_with_kills_and_delays() {
    if skip_external() {
        return;
    }
    // CockroachDB calls take longer, so more calls overlap and each fault catches more of
    // them in flight; fewer clients, shorter histories and sparser faults keep the WGL search
    // within its budget.
    run(
        Backend::External,
        300..306,
        Faults {
            every_ms: (150, 400),
            clients: 4,
            ops: 120,
            ..ALL_FAULTS
        },
    )
    .await;
}
