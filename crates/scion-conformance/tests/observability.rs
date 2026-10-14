//! SPEC 19.17 observability (P30), on the Memory backend and, as `external_*`, on
//! CockroachDB + S3 when the gate variables are set:
//!
//! - After a representative workload through both tiers, every RPC log event (target
//!   `scion::rpc`) carries the SPEC 15.1 required fields, and events of RPCs that touch a
//!   changeset or ref carry `changeset_id`, `ref`, `ref_version` and `land_attempt` where
//!   applicable.
//! - Log scan (PLAN 6.9): `LogCapture::find_needles` finds no Scion token, service secret,
//!   GitHub token, file content or raw (restricted or public) repository path anywhere in the
//!   captured logs of this test binary.
//! - Every SPEC 15.2 metric (plus `scion_integrity_failures_total`, PLAN 6.10) is exported and
//!   moves in its scenario (server `/metrics` and the daemon registry).
//! - `/healthz` and `/readyz` reflect dependency state: an object-store outage and a
//!   metadata-store fault make `/readyz` 503 while `/healthz` stays 200.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use scion_conformance::{Backend, ClusterOpts, TestCluster, Via, invariants};
use scion_core::{ErrorCode, FaultAction, ObjectId, Principal};
use scion_obs::Metrics;
use scion_proto::{ScionClient, v1};
use scion_store::Area;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

fn skip_external() -> bool {
    if Backend::external_available() {
        return false;
    }
    println!("SKIP: SCION_TEST_CRDB_URL / SCION_TEST_S3_ENDPOINT unset");
    true
}

fn gh(login: &str) -> Principal {
    TestCluster::gh(login)
}

fn idb(id: &ObjectId) -> Vec<u8> {
    id.as_bytes().to_vec()
}

/// A per-run unique tag, so needles never collide with other tests in this binary.
fn tag() -> String {
    let n = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:x}", n ^ (u128::from(std::process::id()) << 64))
}

fn opts(backend: Backend) -> ClusterOpts {
    let mut o = ClusterOpts::new(backend);
    o.daemon_tweak = Some(Arc::new(|d| d.redaction_poll_interval_ms = 100));
    o
}

async fn eventually_async<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn land(c: &ScionClient, head: &ObjectId) -> Result<v1::LandResponse, tonic::Status> {
    c.write()
        .land(v1::LandRequest {
            r#ref: "main".into(),
            head: idb(head),
            allow_empty: false,
        })
        .await
        .map(|r| r.into_inner())
}

/// Secrets, contents and paths a workload used; none may appear in any log line.
struct Needles {
    items: Vec<Vec<u8>>,
}

impl Needles {
    fn add(&mut self, b: impl AsRef<[u8]>) {
        self.items.push(b.as_ref().to_vec());
    }
}

/// A representative workload through both tiers: logins (service, GitHub, failed), uploads,
/// changesets with public and restricted paths, reads of every kind (allowed and denied),
/// a conflicting rebase, land, ref updates (incl. a lost CAS), redaction and access checks.
async fn workload(c: &TestCluster, n: &mut Needles) {
    let t = tag();
    let (svc, secret) = TestCluster::SERVICES[0];
    n.add(secret);
    let gh_token = format!("gho_obs_{t}");
    n.add(&gh_token);
    c.github.add_user(&gh_token, "alice", 5150);
    let wrong_secret = format!("wrong-secret-{t}");
    n.add(&wrong_secret);

    for via in Via::ALL {
        let anon = c.client_with_token(via, None).await;
        let r = anon
            .auth()
            .login(v1::LoginRequest {
                credential: Some(v1::login_request::Credential::Service(
                    v1::ServiceCredential {
                        service_name: svc.into(),
                        service_secret: secret.into(),
                    },
                )),
            })
            .await
            .unwrap()
            .into_inner();
        n.add(&r.token);
        let _ = anon
            .auth()
            .login(v1::LoginRequest {
                credential: Some(v1::login_request::Credential::Service(
                    v1::ServiceCredential {
                        service_name: svc.into(),
                        service_secret: wrong_secret.clone(),
                    },
                )),
            })
            .await
            .unwrap_err();
        let r = anon
            .auth()
            .login(v1::LoginRequest {
                credential: Some(v1::login_request::Credential::GithubAccessToken(
                    gh_token.clone(),
                )),
            })
            .await
            .unwrap()
            .into_inner();
        n.add(&r.token);
        let ci = c.client_with_token(via, Some(r.token.clone())).await;
        ci.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
        // A bad token is rejected; it must not be logged either.
        let bad = format!("v4.public.bogus{t}");
        n.add(&bad);
        let _ = c
            .client_with_token(via, Some(bad))
            .await
            .auth()
            .who_am_i(v1::WhoAmIRequest {})
            .await;
    }
    for who in ["alice", "carol", "admin", "mallory"] {
        n.add(c.token_for(&gh(who)));
    }

    let admin = c.client(Via::Server, &gh("admin")).await;
    let public_path = format!("docs/public-{t}.txt");
    let secret_name = format!("plan-{t}.txt");
    let secret_path = format!("secrets/{secret_name}");
    let public_content = format!("public content {t}\nline two\n");
    let secret_content = format!("secret content {t}\nline two\n");
    for x in [
        &public_path,
        &secret_name,
        &secret_path,
        &public_content,
        &secret_content,
    ] {
        n.add(x);
    }
    let main = c.ref_head("main").await.0;
    let cs = TestCluster::commit(
        &admin,
        &main,
        &[
            (&public_path, public_content.as_bytes()),
            (&secret_path, secret_content.as_bytes()),
        ],
        &[],
        1,
    )
    .await;
    land(&admin, &cs).await.unwrap();
    let pub_id = ObjectId::hash_blob(public_content.as_bytes());
    let sec_id = ObjectId::hash_blob(secret_content.as_bytes());

    for via in Via::ALL {
        for who in ["alice", "carol"] {
            let cl = c.client(via, &gh(who)).await;
            let _ = TestCluster::fetch(&cl, &[pub_id, sec_id], None, None)
                .await
                .unwrap();
            let _ = TestCluster::fetch(&cl, &[sec_id], Some(2), Some(5))
                .await
                .unwrap();
            for path in [&public_path, &secret_path] {
                if let Ok(r) = cl
                    .query()
                    .read_path(v1::ReadPathRequest {
                        changeset: idb(&cs),
                        path: path.as_bytes().to_vec(),
                    })
                    .await
                {
                    let mut s = r.into_inner();
                    while let Ok(Some(_)) = s.message().await {}
                }
                let _ = cl
                    .query()
                    .blame(v1::BlameRequest {
                        changeset: idb(&cs),
                        path: path.as_bytes().to_vec(),
                    })
                    .await;
                let _ = cl
                    .query()
                    .log(v1::LogRequest {
                        from: idb(&cs),
                        path: Some(path.as_bytes().to_vec()),
                        first_parent_only: false,
                        limit: 5,
                        page_token: Vec::new(),
                    })
                    .await;
            }
            let _ = cl
                .query()
                .list_tree(v1::ListTreeRequest {
                    changeset: idb(&cs),
                    path: b"secrets".to_vec(),
                })
                .await;
            if let Ok(r) = cl
                .query()
                .diff(v1::DiffRequest {
                    a: idb(&main),
                    b: idb(&cs),
                    paths: Vec::new(),
                    include_content_hunks: true,
                })
                .await
            {
                let mut s = r.into_inner();
                while let Ok(Some(_)) = s.message().await {}
            }
            let _ = cl
                .query()
                .get_changesets(v1::GetChangesetsRequest {
                    ids: vec![idb(&cs)],
                })
                .await;
            let _ = cl
                .objects()
                .has_objects(v1::HasObjectsRequest {
                    ids: vec![idb(&sec_id)],
                })
                .await;
            let _ = cl
                .auth()
                .check_access(v1::CheckAccessRequest {
                    subject_token: c.token_for(&gh("mallory")),
                    object_ids: vec![idb(&sec_id)],
                })
                .await;
        }
        // Writes through each tier: a conflicting rebase (conflict payload with paths) and a
        // land, plus a lost ref CAS.
        let carol = c.client(via, &gh("carol")).await;
        let base = match via {
            Via::Server => 100,
            Via::Daemon => 200,
        };
        let tip = c.ref_head("main").await.0;
        let ours_c = format!("ours {t} {}\n", via.as_str());
        let theirs_c = format!("theirs {t} {}\n", via.as_str());
        n.add(&ours_c);
        n.add(&theirs_c);
        let ours = TestCluster::commit(
            &carol,
            &tip,
            &[(&secret_path, ours_c.as_bytes())],
            &[],
            base + 10,
        )
        .await;
        let theirs = TestCluster::commit(
            &carol,
            &tip,
            &[(&secret_path, theirs_c.as_bytes())],
            &[],
            base + 11,
        )
        .await;
        let _ = carol
            .write()
            .rebase(v1::RebaseRequest {
                changeset: idb(&theirs),
                onto: idb(&ours),
            })
            .await
            .unwrap_err();
        land(&carol, &ours).await.unwrap();
        let (_, v) = c.ref_head("main").await;
        let _ = carol
            .refs()
            .update_ref(v1::UpdateRefRequest {
                name: "main".into(),
                expected_version: v - 1,
                new_target: idb(&ours),
                allow_non_fast_forward: false,
            })
            .await
            .unwrap_err();
        let _ = carol
            .objects()
            .resolve_ref(v1::ResolveRefRequest {
                name: "main".into(),
                at_version: None,
            })
            .await;
    }
    admin
        .admin()
        .redact(v1::RedactRequest {
            object_ids: vec![idb(&pub_id)],
            reason: format!("obs reason {t}"),
            purge_after_seconds: None,
        })
        .await
        .unwrap();
    let carol = c.client(Via::Daemon, &gh("carol")).await;
    let _ = TestCluster::fetch(&carol, &[pub_id], None, None).await;
}

const REQUIRED_FIELDS: &[&str] = &[
    "request_id",
    "rpc",
    "tier",
    "principal",
    "repo",
    "status",
    "duration_ms",
];

async fn run_logs(backend: Backend) {
    let c = TestCluster::start(opts(backend)).await;
    let mut needles = Needles { items: Vec::new() };
    workload(&c, &mut needles).await;
    let events = c.logs.events();
    let rpc: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e.get("target").and_then(|t| t.as_str()) == Some("scion::rpc"))
        .collect();
    assert!(rpc.len() > 50, "the workload produced RPC log events");
    for e in &rpc {
        for f in REQUIRED_FIELDS {
            let v = e
                .get(*f)
                .unwrap_or_else(|| panic!("RPC event lacks {f}: {e}"));
            assert!(!v.is_null(), "RPC event has a null {f}: {e}");
        }
        assert!(e["duration_ms"].is_u64(), "duration_ms is a number: {e}");
        assert!(
            matches!(e["tier"].as_str(), Some("server") | Some("daemon")),
            "tier: {e}"
        );
        assert!(!e["request_id"].as_str().unwrap_or_default().is_empty());
    }
    let has = |rpc_name: &str, tier: &str| {
        rpc.iter()
            .any(|e| e["rpc"] == rpc_name && e["tier"] == tier)
    };
    for (name, t) in [
        ("GetObjects", "daemon"),
        ("GetObjects", "server"),
        ("ResolveRef", "daemon"),
        ("Login", "server"),
        ("Land", "server"),
        ("CreateChangeset", "server"),
    ] {
        assert!(has(name, t), "no {name} event of tier {t}");
    }
    // Fields that apply to particular RPCs (SPEC 15.1).
    let land_ok = rpc
        .iter()
        .find(|e| e["rpc"] == "Land" && e["status"] == "OK")
        .expect("a successful Land event");
    for f in ["changeset_id", "ref", "ref_version", "land_attempt"] {
        assert!(
            land_ok.get(f).is_some_and(|v| !v.is_null()),
            "Land event lacks {f}: {land_ok}"
        );
    }
    let create = rpc
        .iter()
        .find(|e| e["rpc"] == "CreateChangeset" && e["status"] == "OK")
        .expect("a successful CreateChangeset event");
    assert!(create.get("changeset_id").is_some(), "{create}");
    let upd = rpc
        .iter()
        .find(|e| e["rpc"] == "UpdateRef")
        .expect("an UpdateRef event");
    assert!(upd.get("ref").is_some(), "{upd}");
    let anon = rpc
        .iter()
        .find(|e| e["rpc"] == "Login")
        .expect("a Login event");
    assert!(anon["principal"].is_string());

    // PLAN 6.9 log scan over everything captured in this process.
    let refs: Vec<&[u8]> = needles.items.iter().map(|v| v.as_slice()).collect();
    let leaks = c.logs.find_needles(&refs);
    assert!(
        leaks.is_empty(),
        "secrets, contents or paths in logs: {leaks:#?}"
    );
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn log_events_have_required_fields_and_leak_nothing() {
    run_logs(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_log_events_have_required_fields_and_leak_nothing() {
    if skip_external() {
        return;
    }
    run_logs(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Metrics.
// ---------------------------------------------------------------------------------------

fn m(text: &str, name: &str, labels: &[(&str, &str)]) -> f64 {
    TestCluster::metric(text, name, labels).unwrap_or(0.0)
}

fn exported(text: &str, name: &str) -> bool {
    text.lines()
        .any(|l| !l.starts_with('#') && l.starts_with(name))
}

async fn run_metrics(backend: Backend) {
    let c = TestCluster::start(opts(backend)).await;
    let alice = c.client(Via::Server, &gh("alice")).await;
    let alice_d = c.client(Via::Daemon, &gh("alice")).await;
    let admin = c.client(Via::Server, &gh("admin")).await;
    let s0 = c.server_metrics(0).await;
    let d0 = c.daemon_metrics();

    // RPC durations (both tiers).
    alice.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    // Eager derivation on CreateChangeset; land metrics on Land.
    let main = c.ref_head("main").await.0;
    let content = format!("metrics {}\nline\n", tag());
    let big: Vec<u8> = content.repeat(2000).into_bytes();
    let cs = TestCluster::commit(
        &alice,
        &main,
        &[("m/a.txt", content.as_bytes()), ("m/big.txt", &big)],
        &[],
        1,
    )
    .await;
    land(&alice, &cs).await.unwrap();
    // A lost CAS.
    let (_, v) = c.ref_head("main").await;
    alice
        .refs()
        .update_ref(v1::UpdateRefRequest {
            name: "main".into(),
            expected_version: v - 1,
            new_target: idb(&cs),
            allow_non_fast_forward: false,
        })
        .await
        .unwrap_err();
    // Lazy derivation.
    alice
        .query()
        .blame(v1::BlameRequest {
            changeset: idb(&cs),
            path: b"m/a.txt".to_vec(),
        })
        .await
        .unwrap();
    // Objects served by the server, then by the daemon (miss, then hit).
    let blob = ObjectId::hash_blob(content.as_bytes());
    let big_id = ObjectId::hash_blob(&big);
    TestCluster::fetch(&alice, &[blob, big_id], None, None)
        .await
        .unwrap();
    TestCluster::fetch(&alice_d, &[blob, big_id], None, None)
        .await
        .unwrap();
    TestCluster::fetch(&alice_d, &[blob, big_id], None, None)
        .await
        .unwrap();
    // Policy epoch change.
    let changed = format!("{}\n# metrics {}\n", TestCluster::DEFAULT_ACL, tag())
        .replace("[groups]", "[groups]\nextra = [\"github:metrics-user\"]");
    c.set_policy(&changed).unwrap();
    // Redaction (server gauge at once; the daemon after its poll).
    let red = admin
        .admin()
        .redact(v1::RedactRequest {
            object_ids: vec![idb(&blob)],
            reason: "metrics".into(),
            purge_after_seconds: None,
        })
        .await
        .unwrap()
        .into_inner();
    // An integrity failure on the server's own copy.
    let bad_content = format!("corrupt {}\n", tag());
    let bad = TestCluster::commit(
        &admin,
        &cs,
        &[("m/bad.txt", bad_content.as_bytes())],
        &[],
        2,
    )
    .await;
    let _ = bad;
    let bad_id = ObjectId::hash_blob(bad_content.as_bytes());
    let ctx = c.servers[0].ctx();
    let key = ctx.objects.key(&bad_id, Area::Live);
    c.storage
        .blobs
        .put(
            &key,
            Bytes::from(zstd::encode_all(&b"garbage"[..], 3).unwrap()),
        )
        .await
        .unwrap();
    assert!(
        TestCluster::fetch(&alice, &[bad_id], None, None)
            .await
            .is_err()
    );
    c.storage
        .blobs
        .put(
            &key,
            Bytes::from(zstd::encode_all(bad_content.as_bytes(), 3).unwrap()),
        )
        .await
        .unwrap();

    // Wait for the daemon to observe the redaction epoch.
    let d = c.daemon.as_ref().unwrap();
    eventually_async("daemon redaction epoch", || async {
        d.handle.stats().redaction_epoch >= red.epoch
    })
    .await;

    let s1 = c.server_metrics(0).await;
    let d1 = c.daemon_metrics();
    let both = format!("{s1}\n{d1}");
    for name in Metrics::NAMES {
        assert!(exported(&both, name), "{name} is not exported:\n{both}");
    }
    let grew = |before: &str, after: &str, name: &str, labels: &[(&str, &str)]| {
        let (a, b) = (m(before, name, labels), m(after, name, labels));
        assert!(b > a, "{name}{labels:?} did not move ({a} -> {b})");
    };
    grew(
        &s0,
        &s1,
        "scion_rpc_duration_seconds_count",
        &[("rpc", "WhoAmI"), ("tier", "server"), ("code", "OK")],
    );
    grew(
        &d0,
        &d1,
        "scion_rpc_duration_seconds_count",
        &[("rpc", "GetObjects"), ("tier", "daemon")],
    );
    grew(
        &d0,
        &d1,
        "scion_cache_requests_total",
        &[("tier", "daemon"), ("result", "miss")],
    );
    grew(
        &d0,
        &d1,
        "scion_cache_requests_total",
        &[("tier", "daemon"), ("result", "hit")],
    );
    grew(&d0, &d1, "scion_cache_bytes", &[("tier", "daemon")]);
    grew(&s0, &s1, "scion_land_attempts", &[("result", "committed")]);
    grew(&s0, &s1, "scion_ref_cas_failures_total", &[("ref", "main")]);
    grew(&s0, &s1, "scion_land_duration_seconds_count", &[]);
    grew(
        &s0,
        &s1,
        "scion_derive_duration_seconds_count",
        &[("type", "history"), ("mode", "eager")],
    );
    grew(
        &s0,
        &s1,
        "scion_derive_duration_seconds_count",
        &[("type", "blame"), ("mode", "lazy")],
    );
    assert!(
        TestCluster::metric(&s1, "scion_derive_backlog", &[("type", "blame")]).is_some(),
        "the lazy backlog gauge is exported for blame"
    );
    assert_eq!(
        m(&s1, "scion_derive_backlog", &[("type", "blame")]),
        0.0,
        "the backlog drains"
    );
    assert_eq!(
        m(&s1, "scion_redaction_epoch", &[("tier", "server")]),
        red.epoch as f64
    );
    assert!(m(&d1, "scion_redaction_epoch", &[("tier", "daemon")]) >= red.epoch as f64);
    assert!(
        TestCluster::metric(&s1, "scion_redaction_lag_seconds", &[("tier", "server")]).is_some()
    );
    assert!(
        TestCluster::metric(&d1, "scion_redaction_lag_seconds", &[("tier", "daemon")]).is_some()
    );
    let epoch = c.servers[0].ctx().policy.epoch().to_string();
    assert_eq!(
        m(&s1, "scion_policy_epoch_info", &[("epoch", &epoch)]),
        1.0,
        "scion_policy_epoch_info follows the new policy"
    );
    assert!(
        TestCluster::metric(&s0, "scion_policy_epoch_info", &[("epoch", &epoch)]).unwrap_or(0.0)
            == 0.0,
        "the epoch label changed"
    );
    grew(
        &s0,
        &s1,
        "scion_object_bytes_served_total",
        &[("tier", "server")],
    );
    grew(
        &d0,
        &d1,
        "scion_object_bytes_served_total",
        &[("tier", "daemon")],
    );
    grew(
        &d0,
        &d1,
        "scion_upstream_requests_total",
        &[("tier", "daemon")],
    );
    grew(
        &s0,
        &s1,
        "scion_integrity_failures_total",
        &[("tier", "server")],
    );
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn every_metric_is_exported_and_moves() {
    run_metrics(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_every_metric_is_exported_and_moves() {
    if skip_external() {
        return;
    }
    run_metrics(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Health.
// ---------------------------------------------------------------------------------------

async fn run_health(backend: Backend) {
    let c = TestCluster::start(opts(backend)).await;
    let addr = c.servers[0].admin_addr().unwrap();
    let get = |p: &'static str| TestCluster::http_get(addr, p);
    assert_eq!(get("/healthz").await.0, 200);
    assert_eq!(get("/readyz").await.0, 200);

    // Object storage unreachable.
    match c.mem() {
        Some(mem) => mem.blobs.set_down(true),
        None => c.faults().arm(
            "blob.exists.before",
            FaultAction::Error(ErrorCode::Unavailable),
            None,
        ),
    }
    let (code, body) = get("/readyz").await;
    assert_eq!(code, 503, "object store down: {body}");
    assert_eq!(get("/healthz").await.0, 200, "the process is still alive");
    match c.mem() {
        Some(mem) => mem.blobs.set_down(false),
        None => c.faults().disarm_all(),
    }
    assert_eq!(get("/readyz").await.0, 200);

    // Metadata store unreachable (fault point on every backend).
    c.faults().arm(
        "meta.ping.before",
        FaultAction::Error(ErrorCode::Unavailable),
        None,
    );
    let (code, body) = get("/readyz").await;
    assert_eq!(code, 503, "metadata store down: {body}");
    assert_eq!(get("/healthz").await.0, 200);
    c.faults().disarm_all();
    assert_eq!(get("/readyz").await.0, 200);

    // Metrics stay available throughout (SPEC 15.3 listener).
    let (code, body) = get("/metrics").await;
    assert_eq!(code, 200);
    assert!(body.contains("scion_policy_epoch_info"));
}

#[tokio::test(flavor = "multi_thread")]
async fn health_endpoints_reflect_dependency_state() {
    run_health(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_health_endpoints_reflect_dependency_state() {
    if skip_external() {
        return;
    }
    run_health(Backend::External).await;
}
