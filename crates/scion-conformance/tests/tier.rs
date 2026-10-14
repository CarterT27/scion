//! SPEC 19.8 tier conformance (P30), on the Memory backend and, as `external_*`, on
//! CockroachDB + S3 when `SCION_TEST_CRDB_URL` and `SCION_TEST_S3_ENDPOINT` are set:
//!
//! - ONE read-RPC suite (`GetServerInfo`, `GetObjects` with batches, ranges and mixed
//!   statuses, `HasObjects`, `ResolveRef`) runs unchanged with `Via::Server` and `Via::Daemon`
//!   (cold and warm cache); per-item results and decompressed payloads must be byte-identical
//!   for the same principal and epochs (SPEC 7.8).
//! - A scripted sequence of write, query, ref, auth and admin RPCs runs against two identical
//!   clusters, one directly and one through the daemon; the normalized transcripts must be
//!   identical and forwarded responses keep the server's trailers (PLAN 6.13).
//! - Network policy: the daemon dials nothing but its configured upstream.
//! - The P18 `scion_wc::testing` checkout/goto/sparse scenarios re-run against the real
//!   stack through the daemon, checked against the reference model.
//!
//! Every test ends with `invariants::check_16_2`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scion_conformance::{Backend, ClusterOpts, TestCluster, Via, invariants};
use scion_core::{ObjectId, Principal, Tier};
use scion_object::RepoPath;
use scion_proto::v1;
use scion_refmodel::{MTree, file_diff, materialize};
use scion_testkit::{CorpusSpec, generate};
use scion_wc::testing::{self as wct, Scenario, Snapshot};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn gh(login: &str) -> Principal {
    TestCluster::gh(login)
}

/// Cluster options for tier tests: a fast daemon redaction poll so the daemon reaches the
/// server's redaction epoch quickly (both tiers must be compared at equal epochs).
fn opts(backend: Backend) -> ClusterOpts {
    let mut o = ClusterOpts::new(backend);
    o.daemon_tweak = Some(Arc::new(|c| {
        c.redaction_poll_interval_ms = 100;
    }));
    o
}

fn skip_external() -> bool {
    if Backend::external_available() {
        return false;
    }
    println!("SKIP: SCION_TEST_CRDB_URL / SCION_TEST_S3_ENDPOINT unset");
    true
}

/// Poll `f` until it holds (at most 20 s).
async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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

fn status_text(s: &tonic::Status) -> String {
    let d = TestCluster::error_detail(s);
    format!(
        "ERR grpc={:?} code={} retryable={}",
        s.code(),
        d.as_ref().map(|d| d.code.as_str()).unwrap_or("-"),
        d.as_ref().map(|d| d.retryable).unwrap_or(false)
    )
}

/// The objects of the read suite, by label.
struct Fixture {
    named: Vec<(&'static str, ObjectId)>,
}

impl Fixture {
    fn id(&self, name: &str) -> ObjectId {
        self.named.iter().find(|(n, _)| *n == name).unwrap().1
    }

    fn label(&self, id: &ObjectId) -> String {
        self.named
            .iter()
            .find(|(_, i)| i == id)
            .map(|(n, _)| (*n).to_owned())
            .unwrap_or_else(|| id.to_text())
    }

    fn ids(&self) -> Vec<ObjectId> {
        self.named.iter().map(|(_, i)| *i).collect()
    }
}

/// Seed the read-suite repository through the server as `github:admin`: public, shared,
/// restricted, large (multi-chunk), compressible, redacted, unreferenced and absent objects,
/// plus trees and a Meta; `main` is landed to version 2. Waits until the daemon's redaction
/// set reached the server's epoch.
async fn setup_read_fixture(c: &TestCluster) -> Fixture {
    let admin = c.client(Via::Server, &gh("admin")).await;
    let (root, _) = c.ref_head("main").await;
    let big = noise(2 * 1024 * 1024 + 4321, 7);
    let lines: Vec<u8> = b"compressible line of text\n".repeat(20_000);
    let shared = b"shared between a public and a restricted path\n".to_vec();
    let c1 = TestCluster::commit(
        &admin,
        &root,
        &[
            ("README.md", b"# readme\npublic\n"),
            ("data/big.bin", &big),
            ("data/lines.txt", &lines),
            ("docs/shared.txt", &shared),
            ("notes/redact-me.txt", b"to be redacted\n"),
            ("secrets/key.txt", b"restricted key material\n"),
            ("secrets/shared.txt", &shared),
        ],
        &[],
        1,
    )
    .await;
    let landed = admin
        .write()
        .land(v1::LandRequest {
            r#ref: "main".into(),
            head: c1.as_bytes().to_vec(),
            allow_empty: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(landed.version, 2);
    let orphan = TestCluster::upload(&admin, &[b"uploaded but never referenced\n"]).await[0];
    let red = ObjectId::hash_blob(b"to be redacted\n");
    admin
        .admin()
        .redact(v1::RedactRequest {
            object_ids: vec![red.as_bytes().to_vec()],
            reason: "tier test".into(),
            purge_after_seconds: None,
        })
        .await
        .unwrap();
    let tree = |path: &'static str| {
        let admin = admin.clone();
        async move {
            let r = admin
                .query()
                .list_tree(v1::ListTreeRequest {
                    changeset: c1.as_bytes().to_vec(),
                    path: path.as_bytes().to_vec(),
                })
                .await
                .unwrap()
                .into_inner();
            ObjectId::from_bytes(&r.tree_id).unwrap()
        }
    };
    let root_tree = tree("").await;
    let secrets_tree = tree("secrets").await;
    let docs_tree = tree("docs").await;
    let log = admin
        .query()
        .log(v1::LogRequest {
            from: c1.as_bytes().to_vec(),
            path: None,
            first_parent_only: true,
            limit: 1,
            page_token: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();
    let meta = ObjectId::from_bytes(&log.entries[0].meta.as_ref().unwrap().meta_id).unwrap();
    let server_epoch = c.servers[0].ctx().redactions.epoch();
    // The daemon polls ListRedactions with a caller's token: make one authenticated call.
    let via_daemon = c.client(Via::Daemon, &gh("alice")).await;
    via_daemon
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: "main".into(),
            at_version: None,
        })
        .await
        .unwrap();
    let d = c.daemon.as_ref().unwrap();
    eventually("daemon redaction sync", || {
        d.handle.stats().redaction_epoch >= server_epoch
    })
    .await;
    Fixture {
        named: vec![
            ("readme", ObjectId::hash_blob(b"# readme\npublic\n")),
            ("big", ObjectId::hash_blob(&big)),
            ("lines", ObjectId::hash_blob(&lines)),
            ("shared", ObjectId::hash_blob(&shared)),
            ("redacted", red),
            ("secret", ObjectId::hash_blob(b"restricted key material\n")),
            ("orphan", orphan),
            ("absent", ObjectId::hash_blob(b"never uploaded anywhere\n")),
            ("root_tree", root_tree),
            ("secrets_tree", secrets_tree),
            ("docs_tree", docs_tree),
            ("meta", meta),
            ("changeset", c1),
        ],
    }
}

/// One `GetObjects` call rendered as transcript lines (payloads by digest and length).
async fn record_fetch(
    t: &mut Vec<String>,
    f: &Fixture,
    client: &scion_proto::ScionClient,
    label: &str,
    ids: &[ObjectId],
    offset: Option<u64>,
    length: Option<u64>,
) {
    match TestCluster::fetch(client, ids, offset, length).await {
        Ok(r) => {
            let e = TestCluster::epochs(&r.metadata).expect("trailers on GetObjects");
            t.push(format!(
                "{label}: ok, {} items, epochs {}/{}",
                r.items.len(),
                e.policy,
                e.redaction
            ));
            for i in &r.items {
                t.push(format!(
                    "  {} status={} total={} class={} offset={} len={} digest={} verified={}",
                    f.label(&i.id),
                    i.status,
                    i.total_size,
                    i.access_class,
                    i.offset,
                    i.content.len(),
                    ObjectId::hash_blob(&i.content).hex(),
                    i.verified
                ));
            }
        }
        Err(s) => t.push(format!("{label}: {}", status_text(&s))),
    }
}

/// THE read-RPC suite (SPEC 19.8): identical calls for every tier; the transcript holds
/// per-item results, payload digests and the epochs of every response.
async fn read_transcript(c: &TestCluster, via: Via, who: &Principal, f: &Fixture) -> Vec<String> {
    let client = c.client(via, who).await;
    let mut t = Vec::new();

    match client
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
    {
        Ok(r) => {
            let e = TestCluster::epochs(r.metadata()).expect("trailers on GetServerInfo");
            let i = r.into_inner();
            let keys: Vec<String> = i
                .token_public_keys
                .iter()
                .map(|k| format!("{}={}", k.kid, ObjectId::hash_blob(&k.ed25519).hex()))
                .collect();
            t.push(format!(
                "info: versions={:?} repo={} policy={} redaction={} keys={keys:?} limits={:?} gh={} {} trailers {}/{}",
                i.protocol_versions,
                i.repo,
                i.policy_epoch,
                i.redaction_epoch,
                i.limits,
                i.github_client_id,
                i.github_oauth_base_url,
                e.policy,
                e.redaction
            ));
        }
        Err(s) => t.push(format!("info: {}", status_text(&s))),
    }

    let all = f.ids();
    record_fetch(&mut t, f, &client, "batch", &all, None, None).await;
    let mut rev = all.clone();
    rev.reverse();
    record_fetch(&mut t, f, &client, "batch reversed", &rev, None, None).await;
    let dup = [
        f.id("readme"),
        f.id("absent"),
        f.id("readme"),
        f.id("secret"),
    ];
    record_fetch(&mut t, f, &client, "duplicates", &dup, None, None).await;
    for id in &all {
        let label = format!("single {}", f.label(id));
        record_fetch(&mut t, f, &client, &label, &[*id], None, None).await;
    }
    let ranged = [
        f.id("readme"),
        f.id("big"),
        f.id("lines"),
        f.id("secret"),
        f.id("redacted"),
        f.id("absent"),
        f.id("root_tree"),
    ];
    for (off, len) in [
        (Some(1), Some(5)),
        (Some(0), None),
        (None, Some(3)),
        (Some(1 << 30), Some(4)),
        (Some(1_048_570), Some(20)),
        (Some(3), Some(0)),
    ] {
        let label = format!("range {off:?}+{len:?}");
        record_fetch(&mut t, f, &client, &label, &ranged, off, len).await;
    }
    record_fetch(&mut t, f, &client, "empty batch", &[], None, None).await;

    match client
        .objects()
        .has_objects(v1::HasObjectsRequest {
            ids: all.iter().map(|i| i.as_bytes().to_vec()).collect(),
        })
        .await
    {
        Ok(r) => t.push(format!("has: {:?}", r.into_inner().present)),
        Err(s) => t.push(format!("has: {}", status_text(&s))),
    }

    for (name, at) in [
        ("main", None),
        ("main", Some(1)),
        ("main", Some(2)),
        ("main", Some(99)),
        ("no-such-ref", None),
        ("bad..name", None),
        ("", None),
    ] {
        match client
            .objects()
            .resolve_ref(v1::ResolveRefRequest {
                name: name.into(),
                at_version: at,
            })
            .await
        {
            Ok(r) => {
                let e = TestCluster::epochs(r.metadata()).expect("trailers on ResolveRef");
                let r = r.into_inner();
                let target = ObjectId::from_bytes(&r.target).map(|i| f.label(&i));
                t.push(format!(
                    "resolve {name}@{at:?}: {target:?} v{} epochs {}/{}",
                    r.version, e.policy, e.redaction
                ));
            }
            Err(s) => t.push(format!("resolve {name}@{at:?}: {}", status_text(&s))),
        }
    }
    t
}

fn assert_same(server: &[String], other: &[String], what: &str) {
    for (i, (a, b)) in server.iter().zip(other).enumerate() {
        assert_eq!(a, b, "{what}: transcript line {i} differs");
    }
    assert_eq!(server.len(), other.len(), "{what}: transcript length");
}

fn item_line<'a>(t: &'a [String], section: &str, label: &str) -> &'a str {
    let start = t
        .iter()
        .position(|l| l.starts_with(&format!("{section}:")))
        .unwrap_or_else(|| panic!("no section {section}"));
    t[start + 1..]
        .iter()
        .take_while(|l| l.starts_with("  "))
        .find(|l| l.starts_with(&format!("  {label} ")))
        .unwrap_or_else(|| panic!("no item {label} in {section}"))
}

async fn run_read_suite(backend: Backend) {
    let c = TestCluster::start(opts(backend)).await;
    let f = setup_read_fixture(&c).await;
    let ok = v1::ItemStatus::Ok as i32;
    let denied = v1::ItemStatus::Denied as i32;
    let not_found = v1::ItemStatus::NotFound as i32;
    let redacted = v1::ItemStatus::Redacted as i32;
    let root = v1::AccessClass::Root as i32;
    let restricted = v1::AccessClass::Restricted as i32;
    for who in ["alice", "carol", "mallory", "admin"] {
        let p = gh(who);
        let server = read_transcript(&c, Via::Server, &p, &f).await;
        let cold = read_transcript(&c, Via::Daemon, &p, &f).await;
        let warm = read_transcript(&c, Via::Daemon, &p, &f).await;
        assert_same(&server, &cold, &format!("{who}: server vs daemon (cold)"));
        assert_same(&server, &warm, &format!("{who}: server vs daemon (warm)"));

        // The shared transcript must also be right (SPEC 7.3, 12.5, 13.4).
        let line = |label: &str| item_line(&server, "batch", label).to_owned();
        let has = |l: &str, s: i32| l.contains(&format!("status={s} "));
        assert!(has(&line("redacted"), redacted), "{who}: redacted item");
        assert!(
            has(&line("orphan"), not_found),
            "{who}: unreferenced upload"
        );
        assert!(has(&line("absent"), not_found), "{who}: absent id");
        match who {
            "alice" => {
                for l in ["readme", "big", "lines", "shared", "root_tree", "docs_tree"] {
                    let s = line(l);
                    assert!(has(&s, ok), "alice reads {l}: {s}");
                    assert!(s.contains(&format!("class={root} ")), "{l} is root class");
                    assert!(s.contains("verified=true"), "{l} verified");
                }
                assert!(has(&line("secret"), denied), "alice: /secrets is denied");
                assert!(has(&line("secrets_tree"), denied), "alice: secrets tree");
            }
            "carol" | "admin" => {
                let s = line("secret");
                assert!(has(&s, ok) && s.contains(&format!("class={restricted} ")));
                assert!(line("secrets_tree").contains(&format!("class={restricted} ")));
            }
            _ => {
                for l in ["readme", "secret", "shared", "root_tree"] {
                    assert!(has(&line(l), denied), "mallory: {l} denied");
                }
            }
        }
    }
    let d = c.daemon.as_ref().unwrap();
    assert!(
        d.handle.stats().cache_hits > 0,
        "the warm runs hit the cache"
    );
    assert_network_policy(&c);
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn read_suite_is_identical_on_server_and_daemon() {
    run_read_suite(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_read_suite_is_identical_on_server_and_daemon() {
    if skip_external() {
        return;
    }
    run_read_suite(Backend::External).await;
}

/// SPEC 19.8 network policy: the daemon's upstream connector dialed exactly the configured
/// upstream and nothing else.
fn assert_network_policy(c: &TestCluster) {
    let d = c.daemon.as_ref().unwrap();
    let dest = d.handle.upstream().destinations();
    assert_eq!(
        dest,
        vec![c.servers[0].endpoint.to_string()],
        "the daemon contacted something other than its upstream"
    );
    assert_eq!(d.cfg.upstream, c.servers[0].endpoint.to_string());
}

// ---------------------------------------------------------------------------------------
// Non-object RPCs through the daemon behave exactly like direct calls.
// ---------------------------------------------------------------------------------------

struct Script<'a> {
    c: &'a TestCluster,
    via: Via,
    t: Vec<String>,
    names: BTreeMap<Vec<u8>, String>,
}

impl Script<'_> {
    /// Name ids so transcripts stay readable (ids are content-derived, hence identical in
    /// both clusters).
    fn name(&mut self, label: &str, id: &[u8]) {
        self.names.insert(id.to_vec(), label.to_owned());
    }

    fn show(&self, id: &[u8]) -> String {
        if id.is_empty() {
            return "-".into();
        }
        self.names.get(id).cloned().unwrap_or_else(|| {
            ObjectId::from_bytes(id)
                .map(|i| i.to_text())
                .unwrap_or_else(|_| format!("{id:?}"))
        })
    }

    async fn client(&self, who: &str) -> scion_proto::ScionClient {
        self.c.client(self.via, &gh(who)).await
    }

    /// Record a unary result; forwarded responses must carry the server's trailers.
    fn rec<T>(
        &mut self,
        what: &str,
        r: Result<tonic::Response<T>, tonic::Status>,
        show: impl FnOnce(&Self, T) -> String,
    ) -> Option<()> {
        let md = match &r {
            Ok(resp) => resp.metadata().clone(),
            Err(s) => s.metadata().clone(),
        };
        let e = TestCluster::epochs(&md).unwrap_or_else(|| panic!("{what}: no trailers"));
        assert_eq!(
            e.tier,
            Tier::Server,
            "{what}: forwarded RPCs keep scion-tier: server"
        );
        match r {
            Ok(resp) => {
                let line = show(self, resp.into_inner());
                self.t.push(format!(
                    "{what}: {line} epochs {}/{}",
                    e.policy, e.redaction
                ));
                Some(())
            }
            Err(s) => {
                self.t.push(format!("{what}: {}", status_text(&s)));
                None
            }
        }
    }
}

fn first_entry(c: &v1::ChangesetItem) -> String {
    match &c.view {
        Some(v1::changeset_item::View::Encoded(b)) => {
            format!("encoded:{}", ObjectId::hash_blob(b).hex())
        }
        Some(v1::changeset_item::View::FilteredView(v)) => format!(
            "filtered:{} delta={:?}",
            v.filtered,
            v.delta.iter().map(|d| d.path.clone()).collect::<Vec<_>>()
        ),
        None => "none".into(),
    }
}

/// A deterministic sequence of query, write, ref, auth and admin RPCs (SPEC 7.4-7.7)
/// rendered as a transcript without tokens and wall-clock times.
async fn non_object_transcript(c: &TestCluster, via: Via) -> Vec<String> {
    c.github.add_user("gho_tier_token", "alice", 4242);
    let mut s = Script {
        c,
        via,
        t: Vec::new(),
        names: BTreeMap::new(),
    };
    let alice = s.client("alice").await;
    let admin = s.client("admin").await;
    let mallory = s.client("mallory").await;
    let anon = c.client_with_token(via, None).await;
    let (root, _) = c.ref_head("main").await;
    s.name("root", root.as_bytes());

    // Writes: uploads (incl. per-item failures), changesets, validation errors.
    let up = alice
        .write()
        .upload_objects(futures::stream::iter(vec![
            v1::UploadItem {
                id: ObjectId::hash_blob(b"a\n").as_bytes().to_vec(),
                kind: u32::from(scion_core::Kind::Blob.byte()),
                data: b"a\n".to_vec(),
            },
            v1::UploadItem {
                id: ObjectId::hash_blob(b"b\n").as_bytes().to_vec(),
                kind: u32::from(scion_core::Kind::Blob.byte()),
                data: b"not b\n".to_vec(),
            },
        ]))
        .await;
    s.rec("upload", up, |_, r| {
        format!(
            "{:?}",
            r.results
                .iter()
                .map(|x| (x.status, x.error_code.clone()))
                .collect::<Vec<_>>()
        )
    });
    let base = TestCluster::commit(&alice, &root, &[("f.txt", b"one\ntwo\nthree\n")], &[], 1).await;
    s.name("base", base.as_bytes());
    let r = alice
        .write()
        .create_changeset(TestCluster::create_request(
            &base,
            &[("g.txt", b"unknown blob\n")],
            &[],
            2,
        ))
        .await;
    s.rec("create missing blob", r, |_, _| "unexpected".into());
    let r = mallory
        .write()
        .create_changeset(TestCluster::create_request(&base, &[], &["f.txt"], 3))
        .await;
    s.rec("create unwritable", r, |_, _| "unexpected".into());
    let ours =
        TestCluster::commit(&alice, &base, &[("f.txt", b"one\nOURS\nthree\n")], &[], 4).await;
    let theirs =
        TestCluster::commit(&alice, &base, &[("f.txt", b"one\nTHEIRS\nthree\n")], &[], 5).await;
    let other = TestCluster::commit(&alice, &base, &[("h.txt", b"h\n")], &[], 6).await;
    s.name("ours", ours.as_bytes());
    s.name("theirs", theirs.as_bytes());
    s.name("other", other.as_bytes());

    // Rebase and Merge, clean and conflicting.
    let r = alice
        .write()
        .rebase(v1::RebaseRequest {
            changeset: other.as_bytes().to_vec(),
            onto: ours.as_bytes().to_vec(),
        })
        .await;
    s.rec("rebase clean", r, |s, r| s.show(&r.changeset_id));
    let r = alice
        .write()
        .rebase(v1::RebaseRequest {
            changeset: theirs.as_bytes().to_vec(),
            onto: ours.as_bytes().to_vec(),
        })
        .await;
    s.rec("rebase conflict", r, |_, _| "unexpected".into());
    let r = alice
        .write()
        .merge(v1::MergeRequest {
            a: ours.as_bytes().to_vec(),
            b: other.as_bytes().to_vec(),
            meta_fields: Some(TestCluster::meta("merge")),
            change_id: TestCluster::change_id(7),
        })
        .await;
    s.rec("merge clean", r, |s, r| s.show(&r.changeset_id));
    let r = alice
        .write()
        .merge(v1::MergeRequest {
            a: ours.as_bytes().to_vec(),
            b: theirs.as_bytes().to_vec(),
            meta_fields: Some(TestCluster::meta("merge")),
            change_id: TestCluster::change_id(8),
        })
        .await;
    s.rec("merge conflict", r, |_, _| "unexpected".into());

    // Land and refs.
    for (what, head) in [("land", base), ("land retry", base), ("land ours", ours)] {
        let r = alice
            .write()
            .land(v1::LandRequest {
                r#ref: "main".into(),
                head: head.as_bytes().to_vec(),
                allow_empty: false,
            })
            .await;
        s.rec(what, r, |s, r| {
            format!(
                "{} v{} landed={}",
                s.show(&r.target),
                r.version,
                r.landed.len()
            )
        });
    }
    let r = alice
        .write()
        .land(v1::LandRequest {
            r#ref: "main".into(),
            head: theirs.as_bytes().to_vec(),
            allow_empty: false,
        })
        .await;
    s.rec("land conflict", r, |_, _| "unexpected".into());
    let r = alice
        .refs()
        .update_ref(v1::UpdateRefRequest {
            name: "main".into(),
            expected_version: 1,
            new_target: other.as_bytes().to_vec(),
            allow_non_fast_forward: false,
        })
        .await;
    s.rec("update stale", r, |_, _| "unexpected".into());
    let r = alice
        .refs()
        .create_ref(v1::CreateRefRequest {
            name: "feature".into(),
            target: other.as_bytes().to_vec(),
            reason: "x".into(),
        })
        .await;
    s.rec("create ref denied", r, |_, r| format!("v{}", r.version));
    let r = admin
        .refs()
        .create_ref(v1::CreateRefRequest {
            name: "feature".into(),
            target: base.as_bytes().to_vec(),
            reason: "tier".into(),
        })
        .await;
    s.rec("create ref", r, |_, r| format!("v{}", r.version));
    let r = alice
        .refs()
        .update_ref(v1::UpdateRefRequest {
            name: "feature".into(),
            expected_version: 1,
            new_target: other.as_bytes().to_vec(),
            allow_non_fast_forward: false,
        })
        .await;
    s.rec("update ff", r, |_, r| format!("v{}", r.version));
    let r = alice
        .refs()
        .update_ref(v1::UpdateRefRequest {
            name: "feature".into(),
            expected_version: 2,
            new_target: ours.as_bytes().to_vec(),
            allow_non_fast_forward: false,
        })
        .await;
    s.rec("update non-ff", r, |_, _| "unexpected".into());
    let r = admin
        .refs()
        .delete_ref(v1::DeleteRefRequest {
            name: "feature".into(),
            expected_version: 2,
            reason: "tier".into(),
        })
        .await;
    s.rec("delete ref", r, |_, _| "ok".into());
    for name in ["main", "feature"] {
        let r = alice
            .refs()
            .get_ref_log(v1::GetRefLogRequest {
                name: name.into(),
                from_version: 0,
                limit: 0,
            })
            .await;
        s.rec(&format!("ref log {name}"), r, |s, r| {
            r.entries
                .iter()
                .map(|e| {
                    format!(
                        "v{} {}->{} by {} landed={}",
                        e.version,
                        s.show(&e.old_target),
                        s.show(&e.new_target),
                        e.principal,
                        e.landed.len()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        });
    }

    // Queries.
    let (main, _) = c.ref_head("main").await;
    s.name("main-head", main.as_bytes());
    let r = alice
        .query()
        .get_changesets(v1::GetChangesetsRequest {
            ids: vec![
                main.as_bytes().to_vec(),
                ObjectId::hash_blob(b"x").as_bytes().to_vec(),
            ],
        })
        .await;
    s.rec("get changesets", r, |s, r| {
        r.items
            .iter()
            .map(|i| {
                format!(
                    "{} {} {} g{}",
                    s.show(&i.id),
                    i.status,
                    first_entry(i),
                    i.generation
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    });
    let r = alice
        .query()
        .list_tree(v1::ListTreeRequest {
            changeset: main.as_bytes().to_vec(),
            path: Vec::new(),
        })
        .await;
    s.rec("list tree", r, |_, r| {
        format!(
            "{:?}",
            r.entries
                .iter()
                .map(|e| String::from_utf8_lossy(&e.name).into_owned())
                .collect::<Vec<_>>()
        )
    });
    let r = alice
        .query()
        .log(v1::LogRequest {
            from: main.as_bytes().to_vec(),
            path: None,
            first_parent_only: false,
            limit: 0,
            page_token: Vec::new(),
        })
        .await;
    s.rec("log", r, |s, r| {
        r.entries
            .iter()
            .map(|e| s.show(&e.id))
            .collect::<Vec<_>>()
            .join(",")
    });
    let r = alice
        .query()
        .blame(v1::BlameRequest {
            changeset: main.as_bytes().to_vec(),
            path: b"f.txt".to_vec(),
        })
        .await;
    s.rec("blame", r, |s, r| {
        r.ranges
            .iter()
            .map(|b| {
                format!(
                    "{}+{}<-{}@{}",
                    b.start_line,
                    b.line_count,
                    s.show(&b.origin_changeset),
                    b.origin_start_line
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    });
    let r = alice
        .query()
        .is_ancestor(v1::IsAncestorRequest {
            a: root.as_bytes().to_vec(),
            b: main.as_bytes().to_vec(),
        })
        .await;
    s.rec("is ancestor", r, |_, r| r.result.to_string());
    let r = alice
        .query()
        .merge_base(v1::MergeBaseRequest {
            a: ours.as_bytes().to_vec(),
            b: theirs.as_bytes().to_vec(),
        })
        .await;
    s.rec("merge base", r, |s, r| {
        r.bases
            .iter()
            .map(|b| s.show(b))
            .collect::<Vec<_>>()
            .join(",")
    });
    for rev in ["main", "main~1", "nope-ref", "main^^^^^^"] {
        let r = alice
            .query()
            .resolve(v1::ResolveRequest {
                revision: rev.into(),
            })
            .await;
        s.rec(&format!("resolve {rev}"), r, |s, r| s.show(&r.changeset));
    }
    let r = alice
        .query()
        .get_derived(v1::GetDerivedRequest {
            r#type: "no-such-type".into(),
            version: None,
            key: Vec::new(),
        })
        .await;
    s.rec("get derived", r, |_, r| format!("v{}", r.version));
    let r = alice
        .query()
        .diff(v1::DiffRequest {
            a: root.as_bytes().to_vec(),
            b: main.as_bytes().to_vec(),
            paths: Vec::new(),
            include_content_hunks: true,
        })
        .await;
    match r {
        Ok(resp) => {
            let mut stream = resp.into_inner();
            let mut items = Vec::new();
            while let Some(i) = stream.message().await.unwrap() {
                items.push(format!(
                    "{} hunks={}",
                    String::from_utf8_lossy(&i.path),
                    i.hunks.len()
                ));
            }
            let md = stream.trailers().await.unwrap().unwrap_or_default();
            let e = TestCluster::epochs(&md).expect("diff trailers");
            assert_eq!(e.tier, Tier::Server);
            s.t.push(format!("diff: {items:?}"));
        }
        Err(st) => s.t.push(format!("diff: {}", status_text(&st))),
    }
    let r = alice
        .query()
        .read_path(v1::ReadPathRequest {
            changeset: main.as_bytes().to_vec(),
            path: b"f.txt".to_vec(),
        })
        .await;
    match r {
        Ok(resp) => {
            let mut stream = resp.into_inner();
            let mut data = Vec::new();
            while let Some(ch) = stream.message().await.unwrap() {
                data.extend(ch.data);
            }
            s.t.push(format!("read path: {:?}", String::from_utf8_lossy(&data)));
        }
        Err(st) => s.t.push(format!("read path: {}", status_text(&st))),
    }

    // Auth.
    let r = anon
        .auth()
        .login(v1::LoginRequest {
            credential: Some(v1::login_request::Credential::Service(
                v1::ServiceCredential {
                    service_name: "ci".into(),
                    service_secret: TestCluster::SERVICES[0].1.into(),
                },
            )),
        })
        .await;
    s.rec("login service", r, |_, r| r.principal);
    let r = anon
        .auth()
        .login(v1::LoginRequest {
            credential: Some(v1::login_request::Credential::Service(
                v1::ServiceCredential {
                    service_name: "ci".into(),
                    service_secret: "wrong".into(),
                },
            )),
        })
        .await;
    s.rec("login wrong secret", r, |_, r| r.principal);
    let r = anon
        .auth()
        .login(v1::LoginRequest {
            credential: Some(v1::login_request::Credential::GithubAccessToken(
                "gho_tier_token".into(),
            )),
        })
        .await;
    s.rec("login github", r, |_, r| r.principal);
    let r = alice.auth().who_am_i(v1::WhoAmIRequest {}).await;
    s.rec("whoami", r, |_, r| {
        format!("{} {:?}", r.principal, r.groups)
    });
    let r = alice
        .auth()
        .check_access(v1::CheckAccessRequest {
            subject_token: c.token_for(&gh("mallory")),
            object_ids: vec![ObjectId::hash_blob(b"a\n").as_bytes().to_vec()],
        })
        .await;
    s.rec("check access", r, |_, r| {
        format!("{:?} {:?}", r.allowed, r.access_class)
    });

    // Admin.
    let a = ObjectId::hash_blob(b"one\ntwo\nthree\n");
    let ids = vec![a.as_bytes().to_vec()];
    let r = alice
        .admin()
        .redact(v1::RedactRequest {
            object_ids: ids.clone(),
            reason: "r".into(),
            purge_after_seconds: None,
        })
        .await;
    s.rec("redact denied", r, |_, r| format!("e{}", r.epoch));
    let r = admin
        .admin()
        .redact(v1::RedactRequest {
            object_ids: ids.clone(),
            reason: "tier".into(),
            purge_after_seconds: Some(3600),
        })
        .await;
    s.rec("redact", r, |_, r| format!("e{}", r.epoch));
    let r = admin
        .admin()
        .unredact(v1::UnredactRequest {
            object_ids: ids.clone(),
        })
        .await;
    s.rec("unredact", r, |_, r| format!("e{}", r.epoch));
    let r = admin
        .admin()
        .purge_now(v1::PurgeNowRequest {
            object_ids: ids.clone(),
            reason: "tier".into(),
        })
        .await;
    s.rec("purge live", r, |_, _| "ok".into());
    let r = alice
        .admin()
        .list_redactions(v1::ListRedactionsRequest { since_epoch: 0 })
        .await;
    s.rec("list redactions", r, |s, r| {
        format!(
            "{:?} e{}",
            r.entries
                .iter()
                .map(|e| (s.show(&e.object_id), e.state.clone(), e.epoch))
                .collect::<Vec<_>>(),
            r.current_epoch
        )
    });
    s.t
}

async fn run_non_object_rpcs(backend: Backend) {
    let direct = TestCluster::start(opts(backend.clone()).with_daemon(false)).await;
    let proxied = TestCluster::start(opts(backend)).await;
    let a = non_object_transcript(&direct, Via::Server).await;
    let b = non_object_transcript(&proxied, Via::Daemon).await;
    assert_same(&a, &b, "direct vs through the daemon");
    // The script exercised successes and errors of every family.
    for needle in [
        "rebase conflict: ERR grpc=Aborted code=MERGE_CONFLICT",
        "update stale: ERR grpc=Aborted code=REF_VERSION_MISMATCH retryable=true",
        "login wrong secret: ERR grpc=Unauthenticated code=UNAUTHENTICATED",
        "redact denied: ERR grpc=PermissionDenied code=PERMISSION_DENIED",
        "login github: github:alice",
        "upload: [(1, \"\"), (0, \"INTEGRITY\")]",
    ] {
        assert!(
            a.iter().any(|l| l.starts_with(needle)),
            "missing {needle:?} in {a:#?}"
        );
    }
    assert_network_policy(&proxied);
    invariants::check_16_2(&direct).await.unwrap();
    invariants::check_16_2(&proxied).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn non_object_rpcs_through_the_daemon_match_direct_calls() {
    run_non_object_rpcs(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_non_object_rpcs_through_the_daemon_match_direct_calls() {
    if skip_external() {
        return;
    }
    run_non_object_rpcs(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// P18 working-copy scenarios against the real stack through the daemon.
// ---------------------------------------------------------------------------------------

fn snapshot_tree(s: &Snapshot) -> MTree {
    s.iter()
        .map(|(p, (m, c))| {
            (
                p.as_bytes().to_vec(),
                scion_refmodel::MFile {
                    mode: *m as u8,
                    content: c.clone(),
                },
            )
        })
        .collect()
}

fn top_dirs(t: &MTree) -> Vec<Vec<u8>> {
    let mut out: BTreeSet<Vec<u8>> = BTreeSet::new();
    for p in t.keys() {
        if let Some(i) = p.iter().position(|b| *b == b'/') {
            out.insert(p[..i].to_vec());
        }
    }
    out.into_iter().collect()
}

async fn create_ref(c: &TestCluster, name: &str, target: &ObjectId) {
    let admin = c.client(Via::Server, &gh("admin")).await;
    admin
        .refs()
        .create_ref(v1::CreateRefRequest {
            name: name.into(),
            target: target.as_bytes().to_vec(),
            reason: "scenario".into(),
        })
        .await
        .unwrap();
}

async fn run_checkout_scenarios(backend: Backend) {
    let c = TestCluster::start(opts(backend)).await;
    let repo = generate(&CorpusSpec {
        files: 60,
        depth: 3,
        fanout: 3,
        history_len: 10,
        seed: 30,
        ..CorpusSpec::default()
    });
    assert!(
        repo.commits
            .iter()
            .all(|g| g.tree.keys().all(|p| !p.starts_with(b"secrets"))),
        "generated paths stay outside the restricted prefix"
    );
    let ids = c.seed(&repo).await;
    let (mid, last) = (repo.commits.len() / 2, repo.commits.len() - 1);
    create_ref(&c, "gen-a", &ids[mid]).await;
    create_ref(&c, "gen-b", &ids[last]).await;

    // Hand-made pair for the dirty-file protection scenario.
    let alice_direct = c.client(Via::Server, &gh("alice")).await;
    let v1c = TestCluster::commit(
        &alice_direct,
        &ids[last],
        &[
            ("zz/touched.txt", b"one\n"),
            ("zz/unrelated.txt", b"same\n"),
        ],
        &[],
        900,
    )
    .await;
    let v2c = TestCluster::commit(
        &alice_direct,
        &v1c,
        &[("zz/touched.txt", b"two\n")],
        &[],
        901,
    )
    .await;
    create_ref(&c, "v1", &v1c).await;
    create_ref(&c, "v2", &v2c).await;

    let s = Scenario::new(c.client(Via::Daemon, &gh("alice")).await).unwrap();
    let tree_a = &repo.commits[mid].tree;
    let tree_b = &repo.commits[last].tree;

    // Full and sparse checkouts equal the model's materialization (SPEC 19.10).
    for (rev, tree) in [("gen-a", tree_a), ("gen-b", tree_b)] {
        let snap = wct::checkout_snapshot(&s, rev, &[]).await.unwrap();
        assert_eq!(snapshot_tree(&snap), *tree, "{rev}: full checkout");
        for dir in top_dirs(tree).into_iter().take(2) {
            let rp = RepoPath::parse(&dir).unwrap();
            let snap = wct::checkout_snapshot(&s, rev, &[rp]).await.unwrap();
            assert_eq!(
                snapshot_tree(&snap),
                materialize(tree, std::slice::from_ref(&dir)),
                "{rev}: sparse checkout of {}",
                String::from_utf8_lossy(&dir)
            );
        }
    }

    // A clean status after a warm checkout makes zero calls through the daemon.
    let (calls, hashed) = wct::clean_status_zero_calls(&s, "gen-b").await.unwrap();
    assert_eq!((calls, hashed), (0, 0));

    // goto touches exactly the model's diff.
    let changed = wct::goto_touches_only_diff(&s, "gen-a", "gen-b")
        .await
        .unwrap();
    let mut want: Vec<RepoPath> = file_diff(tree_a, tree_b)
        .into_iter()
        .map(|d| match d {
            scion_refmodel::MDelta::Put(p, _) | scion_refmodel::MDelta::Delete(p) => {
                RepoPath::parse(&p).unwrap()
            }
        })
        .collect();
    want.sort();
    assert_eq!(changed, want, "goto touched exactly the changed paths");

    wct::goto_dirty_protection(
        &s,
        "v1",
        "v2",
        &RepoPath::parse(b"zz/touched.txt").unwrap(),
        &RepoPath::parse(b"zz/unrelated.txt").unwrap(),
    )
    .await
    .unwrap();

    let dirs = top_dirs(tree_b);
    assert!(dirs.len() >= 2, "the generated tree has two top-level dirs");
    wct::sparse_add_remove(
        &s,
        "gen-b",
        &RepoPath::parse(&dirs[0]).unwrap(),
        &RepoPath::parse(&dirs[1]).unwrap(),
    )
    .await
    .unwrap();

    for p in wct::materialization_points() {
        wct::goto_crash_recovery(&s, "gen-a", "gen-b", p)
            .await
            .unwrap_or_else(|e| panic!("crash recovery at {p}: {e}"));
    }

    // A sparse fetch requests only objects within the scope (SPEC 10.1).
    let scope = RepoPath::parse(&dirs[0]).unwrap();
    let fetched = wct::sparse_object_set(&s, "gen-b", &scope).await.unwrap();
    let inside: BTreeSet<ObjectId> = tree_b
        .iter()
        .filter(|(p, _)| p.starts_with(&dirs[0]) && p.get(dirs[0].len()) == Some(&b'/'))
        .map(|(_, f)| ObjectId::hash_blob(&f.content))
        .collect();
    let fetched_blobs: BTreeSet<ObjectId> = fetched
        .iter()
        .filter(|id| id.kind() == scion_core::Kind::Blob)
        .copied()
        .collect();
    assert_eq!(
        fetched_blobs, inside,
        "a sparse fetch gets exactly the in-scope blobs"
    );

    let d = c.daemon.as_ref().unwrap();
    assert!(
        d.handle.stats().cache_hits > 0,
        "scenarios were served from the daemon cache"
    );
    assert_network_policy(&c);
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn working_copy_scenarios_through_the_daemon() {
    run_checkout_scenarios(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_working_copy_scenarios_through_the_daemon() {
    if skip_external() {
        return;
    }
    run_checkout_scenarios(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Harness self-check: several servers over one storage (used by the multi-server suites).
// ---------------------------------------------------------------------------------------

async fn run_multi_server(backend: Backend) {
    let mut c = TestCluster::start(opts(backend).with_servers(2)).await;
    let alice = gh("alice");
    let token = c.token_for(&alice);
    let s0 = c.client(Via::Server, &alice).await;
    let s1 = scion_proto::ScionClient::connect(
        &c.servers[1].endpoint,
        c.servers[1].tls.clone(),
        Some(token.clone()),
    )
    .await
    .unwrap();
    let (root, _) = c.ref_head("main").await;
    let cs = TestCluster::commit(&s0, &root, &[("multi/a.txt", b"a\n")], &[], 1).await;
    // The second server sees the first server's writes and accepts the shared token.
    let r = s1
        .query()
        .get_changesets(v1::GetChangesetsRequest {
            ids: vec![cs.as_bytes().to_vec()],
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.items[0].status, v1::ItemStatus::Ok as i32);
    let before = c.counters();
    c.kill_server(1).await;
    assert!(s1.auth().who_am_i(v1::WhoAmIRequest {}).await.is_err());
    c.restart_server(1).await;
    let s1 = scion_proto::ScionClient::connect(
        &c.servers[1].endpoint,
        c.servers[1].tls.clone(),
        Some(token),
    )
    .await
    .unwrap();
    let who = s1.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    assert_eq!(who.into_inner().principal, "github:alice");
    let f = TestCluster::fetch(&s1, &[ObjectId::hash_blob(b"a\n")], None, None)
        .await
        .unwrap();
    assert!(f.items[0].verified);
    let delta = c.counters() - before;
    assert!(
        delta.meta_txns > 0,
        "the restarted server reads the shared storage"
    );
    // A policy change applies to every server.
    let acl = TestCluster::DEFAULT_ACL.replace("\"github:alice\", ", "");
    assert_ne!(acl, TestCluster::DEFAULT_ACL);
    c.set_policy(&acl).unwrap();
    for s in [&s0, &s1] {
        let e = s
            .query()
            .get_changesets(v1::GetChangesetsRequest {
                ids: vec![cs.as_bytes().to_vec()],
            })
            .await
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
    }
    assert!(
        c.set_policy("not toml [").is_err(),
        "invalid policy is rejected"
    );
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cluster_servers_share_storage_keys_and_policy() {
    run_multi_server(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_cluster_servers_share_storage_keys_and_policy() {
    if skip_external() {
        return;
    }
    run_multi_server(Backend::External).await;
}

// ---------------------------------------------------------------------------------------
// Harness self-test: the SPEC 16.2 oracle detects what it claims to detect.
// ---------------------------------------------------------------------------------------

async fn run_invariants_detect_violations(backend: Backend) {
    let mut o = opts(backend);
    // Without verify_on_read the daemon would serve a corrupted cache file, so the oracle
    // must report it (with verification on, such an entry is discarded, never served).
    o.daemon_tweak = Some(Arc::new(|d| {
        d.redaction_poll_interval_ms = 100;
        d.verify_on_read = false;
    }));
    let c = TestCluster::start(o).await;
    let admin = c.client(Via::Server, &gh("admin")).await;
    let (root, _) = c.ref_head("main").await;
    let content = b"oracle self-test content\n".to_vec();
    TestCluster::commit(&admin, &root, &[("oracle/a.txt", &content)], &[], 1).await;
    let id = ObjectId::hash_blob(&content);
    let alice = c.client(Via::Daemon, &gh("alice")).await;
    let f = TestCluster::fetch(&alice, &[id], None, None).await.unwrap();
    assert!(f.items[0].verified, "served through the daemon");
    invariants::check_16_2(&c).await.unwrap();

    // (3) A daemon cache entry whose bytes do not match its id.
    let d = c.daemon.as_ref().unwrap();
    let path = d.handle.cache().entry_path_of(&id);
    assert!(path.exists(), "the daemon cached the object at {path:?}");
    let original = std::fs::read(&path).unwrap();
    let mut tampered = vec![0u8];
    tampered.extend_from_slice(b"oracle self-test CONTENT\n");
    std::fs::write(&path, &tampered).unwrap();
    let e = invariants::check_16_2(&c).await.unwrap_err();
    assert_eq!(e.code, scion_core::ErrorCode::Integrity);
    assert!(e.message.contains("daemon cache entry"), "{e:?}");
    std::fs::write(&path, &original).unwrap();
    invariants::check_16_2(&c).await.unwrap();

    // (1) The server's stored copy no longer matches its id (fsck with verify_bytes).
    let key = c.servers[0].ctx().objects.key(&id, scion_store::Area::Live);
    let good = c.storage.blobs.get(&key).await.unwrap().unwrap();
    c.storage
        .blobs
        .put(
            &key,
            bytes::Bytes::from(zstd::encode_all(&b"not the content"[..], 3).unwrap()),
        )
        .await
        .unwrap();
    let e = invariants::check_16_2(&c).await.unwrap_err();
    assert!(e.message.contains("fsck"), "{e:?}");
    c.storage.blobs.put(&key, good).await.unwrap();
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn invariants_detect_violations() {
    run_invariants_detect_violations(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_invariants_detect_violations() {
    if skip_external() {
        return;
    }
    run_invariants_detect_violations(Backend::External).await;
}
