//! SPEC 19.12 access-control matrix (P30): every read and query RPC (enumerated from the
//! descriptor: ObjectService and QueryService) x {allowed, denied at `/`, denied at a
//! subdirectory} x {server, daemon} returns the specified result, on the Memory backend and,
//! as `external_*`, on CockroachDB + S3 when the gate variables are set.
//!
//! Principals under [`TestCluster::DEFAULT_ACL`]: `github:carol` (security) reads `/` and
//! `/secrets` (allowed), `github:mallory` has no grant (denied at `/`), `github:alice` (eng)
//! reads `/` but not `/secrets` (denied at a subdirectory). Requests target restricted
//! content so the three scenarios differ. For denied principals every response is also
//! scanned for the restricted file name and content (SPEC 12.6: never returned to
//! unauthorized principals). Specified results (SPEC 12.4-12.6, PLAN 6.12): object reads per
//! item (`denied`, never failing the batch); `ResolveRef` needs only a valid token (refs are
//! not path-scoped); repository-level queries need read on `/`; path queries need read on
//! the path; `GetChangesets`, `Log` and `Diff` without a path filter unreadable entries.
//!
//! Additionally (SPEC 19.12 daemon item): a restricted object cached by the daemon for an
//! allowed principal is not served to a denied one.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use prost::Message;
use scion_conformance::{Backend, ClusterOpts, TestCluster, Via, invariants};
use scion_core::{ObjectId, Principal, Tier};
use scion_proto::{ScionClient, v1};
use std::time::{Duration, Instant};

const SECRET_NAME: &str = "plan-q7x9.txt";
const SECRET_CONTENT: &[u8] = b"TOP SECRET CONTENT 5f2e19d4\nsecond line\n";
const PUBLIC_CONTENT: &[u8] = b"public readme\nline two\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Allowed,
    DeniedAtRoot,
    DeniedAtSubdir,
}

impl Scenario {
    const ALL: [Scenario; 3] = [
        Scenario::Allowed,
        Scenario::DeniedAtRoot,
        Scenario::DeniedAtSubdir,
    ];

    fn principal(self) -> Principal {
        TestCluster::gh(match self {
            Scenario::Allowed => "carol",
            Scenario::DeniedAtRoot => "mallory",
            Scenario::DeniedAtSubdir => "alice",
        })
    }
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

fn secret_path() -> String {
    format!("secrets/{SECRET_NAME}")
}

struct Fixture {
    root: ObjectId,
    /// Adds README.md and docs/a.txt.
    c1: ObjectId,
    /// Adds secrets/<SECRET_NAME> and edits README.md.
    c2: ObjectId,
    public: ObjectId,
    secret: ObjectId,
    secrets_tree: ObjectId,
}

async fn setup(c: &TestCluster) -> Fixture {
    let admin = c.client(Via::Server, &TestCluster::gh("admin")).await;
    let (root, _) = c.ref_head("main").await;
    let c1 = TestCluster::commit(
        &admin,
        &root,
        &[("README.md", b"readme v1\n"), ("docs/a.txt", b"a\n")],
        &[],
        1,
    )
    .await;
    let sp = secret_path();
    let c2 = TestCluster::commit(
        &admin,
        &c1,
        &[("README.md", PUBLIC_CONTENT), (&sp, SECRET_CONTENT)],
        &[],
        2,
    )
    .await;
    admin
        .write()
        .land(v1::LandRequest {
            r#ref: "main".into(),
            head: idb(&c2),
            allow_empty: false,
        })
        .await
        .unwrap();
    let secrets_tree = ObjectId::from_bytes(
        &admin
            .query()
            .list_tree(v1::ListTreeRequest {
                changeset: idb(&c2),
                path: b"secrets".to_vec(),
            })
            .await
            .unwrap()
            .into_inner()
            .tree_id,
    )
    .unwrap();
    Fixture {
        root,
        c1,
        c2,
        public: ObjectId::hash_blob(PUBLIC_CONTENT),
        secret: ObjectId::hash_blob(SECRET_CONTENT),
        secrets_tree,
    }
}

/// SPEC 12.6: nothing returned to a principal that cannot read `/secrets` may contain the
/// restricted file name or content.
fn no_leak(s: Scenario, what: &str, bytes: &[u8]) {
    if s == Scenario::Allowed {
        return;
    }
    for needle in [
        SECRET_NAME.as_bytes(),
        SECRET_CONTENT,
        &SECRET_CONTENT[..16],
    ] {
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle),
            "{what}: restricted data leaked to {s:?}"
        );
    }
}

fn tier(via: Via, local: bool) -> Tier {
    match (via, local) {
        (Via::Daemon, true) => Tier::Daemon,
        _ => Tier::Server,
    }
}

/// Unary outcome: the value (trailers checked) or PERMISSION_DENIED (trailers checked).
fn unary<T: Message>(
    s: Scenario,
    what: &str,
    r: Result<tonic::Response<T>, tonic::Status>,
    t: Tier,
) -> Option<T> {
    match r {
        Ok(resp) => {
            let e = TestCluster::epochs(resp.metadata()).expect("trailers");
            assert_eq!(e.tier, t, "{what}: tier");
            let v = resp.into_inner();
            no_leak(s, what, &v.encode_to_vec());
            Some(v)
        }
        Err(st) => {
            assert_eq!(
                st.code(),
                tonic::Code::PermissionDenied,
                "{what} ({s:?}): {st:?}"
            );
            let e = TestCluster::epochs(st.metadata()).expect("trailers on errors");
            assert_eq!(e.tier, t, "{what}: tier");
            no_leak(s, what, st.message().as_bytes());
            no_leak(s, what, st.details());
            None
        }
    }
}

/// Streaming outcome: every message, or PERMISSION_DENIED.
async fn streaming<T: Message>(
    s: Scenario,
    what: &str,
    r: Result<tonic::Response<tonic::Streaming<T>>, tonic::Status>,
) -> Option<Vec<T>> {
    let st = match r {
        Ok(resp) => {
            let mut stream = resp.into_inner();
            let mut out = Vec::new();
            loop {
                match stream.message().await {
                    Ok(Some(m)) => {
                        no_leak(s, what, &m.encode_to_vec());
                        out.push(m);
                    }
                    Ok(None) => {
                        let md = stream.trailers().await.unwrap().unwrap_or_default();
                        assert!(TestCluster::epochs(&md).is_some(), "{what}: trailers");
                        return Some(out);
                    }
                    Err(st) => break st,
                }
            }
        }
        Err(st) => st,
    };
    assert_eq!(
        st.code(),
        tonic::Code::PermissionDenied,
        "{what} ({s:?}): {st:?}"
    );
    assert!(
        TestCluster::epochs(st.metadata()).is_some(),
        "{what}: trailers"
    );
    no_leak(s, what, st.message().as_bytes());
    None
}

fn expect_some<T>(s: Scenario, what: &str, v: Option<T>, allowed: bool) -> Option<T> {
    match (&v, allowed) {
        (Some(_), true) | (None, false) => v,
        (Some(_), false) => panic!("{what}: {s:?} must be denied"),
        (None, true) => panic!("{what}: {s:?} must be allowed"),
    }
}

/// Run every case of `method` for scenario `s` through `via`; panics on a wrong result.
/// Returns false when the method has no case (the caller fails the matrix).
async fn run_case(method: &str, s: Scenario, via: Via, c: &ScionClient, f: &Fixture) -> bool {
    let reads_root = s != Scenario::DeniedAtRoot;
    let reads_secrets = s == Scenario::Allowed;
    let w = |x: &str| format!("{method} {x} via {}", via.as_str());
    match method {
        "GetServerInfo" => {
            let v = unary(
                s,
                &w(""),
                c.objects()
                    .get_server_info(v1::GetServerInfoRequest {})
                    .await,
                tier(via, true),
            );
            assert!(v.is_some(), "GetServerInfo is open to everyone");
        }
        "GetObjects" => {
            let r = TestCluster::fetch(c, &[f.public, f.secret, f.secrets_tree], None, None)
                .await
                .unwrap();
            let e = TestCluster::epochs(&r.metadata).expect("trailers");
            assert_eq!(e.tier, tier(via, true));
            for i in &r.items {
                no_leak(s, &w("item"), &i.content);
            }
            let st: Vec<i32> = r.items.iter().map(|i| i.status).collect();
            let okv = v1::ItemStatus::Ok as i32;
            let den = v1::ItemStatus::Denied as i32;
            let want = match s {
                Scenario::Allowed => vec![okv, okv, okv],
                Scenario::DeniedAtRoot => vec![den, den, den],
                Scenario::DeniedAtSubdir => vec![okv, den, den],
            };
            assert_eq!(st, want, "{}", w("statuses"));
            if s == Scenario::Allowed {
                assert_eq!(r.items[0].access_class, v1::AccessClass::Root as i32);
                assert_eq!(r.items[1].access_class, v1::AccessClass::Restricted as i32);
                assert!(r.items.iter().all(|i| i.verified));
            }
            // Range reads follow the same decisions.
            let r = TestCluster::fetch(c, &[f.secret], Some(0), Some(8))
                .await
                .unwrap();
            assert_eq!(r.items[0].status, if reads_secrets { okv } else { den });
            no_leak(s, &w("range"), &r.items[0].content);
        }
        "HasObjects" => {
            let v = unary(
                s,
                &w(""),
                c.objects()
                    .has_objects(v1::HasObjectsRequest {
                        ids: vec![idb(&f.public), idb(&f.secret)],
                    })
                    .await,
                tier(via, false),
            )
            .unwrap();
            assert_eq!(v.present, vec![reads_root, reads_secrets], "{}", w(""));
        }
        "ResolveRef" => {
            let v = unary(
                s,
                &w(""),
                c.objects()
                    .resolve_ref(v1::ResolveRefRequest {
                        name: "main".into(),
                        at_version: None,
                    })
                    .await,
                tier(via, true),
            );
            assert_eq!(v.unwrap().target, idb(&f.c2), "refs are not path-scoped");
        }
        "GetChangesets" => {
            let v = unary(
                s,
                &w(""),
                c.query()
                    .get_changesets(v1::GetChangesetsRequest {
                        ids: vec![idb(&f.c1), idb(&f.c2)],
                    })
                    .await,
                tier(via, false),
            );
            if let Some(v) = expect_some(s, &w(""), v, reads_root) {
                assert!(matches!(
                    v.items[0].view,
                    Some(v1::changeset_item::View::Encoded(_))
                ));
                match (&v.items[1].view, reads_secrets) {
                    (Some(v1::changeset_item::View::Encoded(_)), true) => {}
                    (Some(v1::changeset_item::View::FilteredView(fv)), false) => {
                        assert!(fv.filtered);
                        assert_eq!(
                            fv.delta.iter().map(|d| d.path.clone()).collect::<Vec<_>>(),
                            vec![b"README.md".to_vec()]
                        );
                    }
                    other => panic!("{}: unexpected view {other:?}", w("")),
                }
            }
        }
        "ReadPath" => {
            for (path, allowed) in [
                ("README.md", reads_root),
                (secret_path().as_str(), reads_secrets),
            ] {
                let v = streaming(
                    s,
                    &w(path),
                    c.query()
                        .read_path(v1::ReadPathRequest {
                            changeset: idb(&f.c2),
                            path: path.as_bytes().to_vec(),
                        })
                        .await,
                )
                .await;
                if let Some(chunks) = expect_some(s, &w(path), v, allowed) {
                    let data: Vec<u8> = chunks.into_iter().flat_map(|c| c.data).collect();
                    assert!(!data.is_empty());
                }
            }
        }
        "ListTree" => {
            for (path, allowed) in [("", reads_root), ("secrets", reads_secrets)] {
                let v = unary(
                    s,
                    &w(path),
                    c.query()
                        .list_tree(v1::ListTreeRequest {
                            changeset: idb(&f.c2),
                            path: path.as_bytes().to_vec(),
                        })
                        .await,
                    tier(via, false),
                );
                if let Some(v) = expect_some(s, &w(path), v, allowed)
                    && path.is_empty()
                {
                    // SPEC 12.6: the restricted directory is listed (name and id).
                    assert!(v.entries.iter().any(|e| e.name == b"secrets"));
                }
            }
        }
        "Log" => {
            let req = |path: Option<String>| v1::LogRequest {
                from: idb(&f.c2),
                path: path.map(String::into_bytes),
                first_parent_only: false,
                limit: 10,
                page_token: Vec::new(),
            };
            let v = unary(
                s,
                &w("all"),
                c.query().log(req(None)).await,
                tier(via, false),
            );
            if let Some(v) = expect_some(s, &w("all"), v, reads_root) {
                let ids: Vec<Vec<u8>> = v.entries.iter().map(|e| e.id.clone()).collect();
                assert_eq!(ids, vec![idb(&f.c2), idb(&f.c1), idb(&f.root)]);
            }
            let v = unary(
                s,
                &w("secret path"),
                c.query().log(req(Some(secret_path()))).await,
                tier(via, false),
            );
            if let Some(v) = expect_some(s, &w("secret path"), v, reads_secrets) {
                assert_eq!(v.entries.len(), 1);
            }
            let v = unary(
                s,
                &w("public path"),
                c.query().log(req(Some("README.md".into()))).await,
                tier(via, false),
            );
            expect_some(s, &w("public path"), v, reads_root);
        }
        "Blame" => {
            for (path, allowed) in [
                ("README.md", reads_root),
                (secret_path().as_str(), reads_secrets),
            ] {
                let v = unary(
                    s,
                    &w(path),
                    c.query()
                        .blame(v1::BlameRequest {
                            changeset: idb(&f.c2),
                            path: path.as_bytes().to_vec(),
                        })
                        .await,
                    tier(via, false),
                );
                expect_some(s, &w(path), v, allowed);
            }
        }
        "Diff" => {
            let req = |paths: Vec<&str>| v1::DiffRequest {
                a: idb(&f.c1),
                b: idb(&f.c2),
                paths: paths.into_iter().map(|p| p.as_bytes().to_vec()).collect(),
                include_content_hunks: true,
            };
            let v = streaming(s, &w("all"), c.query().diff(req(vec![])).await).await;
            if let Some(items) = expect_some(s, &w("all"), v, reads_root) {
                let paths: Vec<Vec<u8>> = items.iter().map(|i| i.path.clone()).collect();
                let mut want = vec![b"README.md".to_vec()];
                if reads_secrets {
                    want.push(secret_path().into_bytes());
                }
                assert_eq!(paths, want, "{}", w("all"));
            }
            let v = streaming(s, &w("secrets"), c.query().diff(req(vec!["secrets"])).await).await;
            expect_some(s, &w("secrets"), v, reads_secrets);
        }
        "IsAncestor" => {
            let v = unary(
                s,
                &w(""),
                c.query()
                    .is_ancestor(v1::IsAncestorRequest {
                        a: idb(&f.c1),
                        b: idb(&f.c2),
                    })
                    .await,
                tier(via, false),
            );
            if let Some(v) = expect_some(s, &w(""), v, reads_root) {
                assert!(v.result);
            }
        }
        "MergeBase" => {
            let v = unary(
                s,
                &w(""),
                c.query()
                    .merge_base(v1::MergeBaseRequest {
                        a: idb(&f.c1),
                        b: idb(&f.c2),
                    })
                    .await,
                tier(via, false),
            );
            if let Some(v) = expect_some(s, &w(""), v, reads_root) {
                assert_eq!(v.bases, vec![idb(&f.c1)]);
            }
        }
        "Resolve" => {
            let v = unary(
                s,
                &w(""),
                c.query()
                    .resolve(v1::ResolveRequest {
                        revision: "main~1".into(),
                    })
                    .await,
                tier(via, false),
            );
            if let Some(v) = expect_some(s, &w(""), v, reads_root) {
                assert_eq!(v.changeset, idb(&f.c1));
            }
        }
        "GetDerived" => {
            let v = unary(
                s,
                &w(""),
                c.query()
                    .get_derived(v1::GetDerivedRequest {
                        r#type: "history".into(),
                        version: None,
                        key: idb(&f.c2),
                    })
                    .await,
                tier(via, false),
            );
            expect_some(s, &w(""), v, reads_root);
        }
        _ => return false,
    }
    true
}

async fn run_matrix(backend: Backend) {
    let c = TestCluster::start(ClusterOpts::new(backend)).await;
    let f = setup(&c).await;
    let rpcs: Vec<_> = TestCluster::rpcs()
        .into_iter()
        .filter(|r| r.service == "ObjectService" || r.service == "QueryService")
        .collect();
    assert_eq!(rpcs.len(), 14, "4 object + 10 query RPCs");
    // The allowed principal runs first so the daemon cache holds the restricted objects when
    // the denied principals ask (SPEC 10.4: never served on another principal's decision).
    for s in Scenario::ALL {
        for via in Via::ALL {
            let client = c.client(via, &s.principal()).await;
            for rpc in &rpcs {
                assert!(
                    run_case(&rpc.method, s, via, &client, &f).await,
                    "{} has no access-matrix case",
                    rpc.path
                );
            }
        }
    }
    let d = c.daemon.as_ref().unwrap();
    assert!(
        d.handle.cache().contains(&f.secret),
        "the daemon cached the secret"
    );
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn read_and_query_access_matrix() {
    run_matrix(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_read_and_query_access_matrix() {
    if skip_external() {
        return;
    }
    run_matrix(Backend::External).await;
}

/// SPEC 19.12 daemon item: a restricted object cached for one principal is served to another
/// only on that principal's own positive decision, and a policy epoch change drops cached
/// decisions.
async fn run_daemon_decisions(backend: Backend) {
    // Decisions never expire during the test, so only the SPEC 10.4 epoch rule (drop every
    // decision when a response carries a different policy epoch) can change the outcome.
    let mut o = ClusterOpts::new(backend);
    o.daemon_tweak = Some(std::sync::Arc::new(|d| d.access_cache_ttl_ms = 3_600_000));
    let c = TestCluster::start(o).await;
    let f = setup(&c).await;
    let carol = c.client(Via::Daemon, &TestCluster::gh("carol")).await;
    let alice = c.client(Via::Daemon, &TestCluster::gh("alice")).await;
    let ok = v1::ItemStatus::Ok as i32;
    let denied = v1::ItemStatus::Denied as i32;
    let r = TestCluster::fetch(&carol, &[f.secret], None, None)
        .await
        .unwrap();
    assert_eq!(r.items[0].status, ok);
    let r = TestCluster::fetch(&alice, &[f.secret], None, None)
        .await
        .unwrap();
    assert_eq!(r.items[0].status, denied, "carol's decision is not alice's");
    // Grant alice /secrets: the new policy epoch reaches the daemon with the next response
    // and drops its decisions, so alice is now served.
    let granting = TestCluster::DEFAULT_ACL.replace(
        "read = [\"group:security\", \"group:admins\"]",
        "read = [\"group:security\", \"group:admins\", \"github:alice\"]",
    );
    assert_ne!(granting, TestCluster::DEFAULT_ACL);
    c.set_policy(&granting).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        // Any forwarded response carries the new epoch.
        alice.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
        let r = TestCluster::fetch(&alice, &[f.secret], None, None)
            .await
            .unwrap();
        if r.items[0].status == ok {
            assert!(r.items[0].verified);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the policy change never reached the daemon"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Revoke again: the cached positive decision is dropped as well.
    c.set_policy(TestCluster::DEFAULT_ACL).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        alice.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
        let r = TestCluster::fetch(&alice, &[f.secret], None, None)
            .await
            .unwrap();
        if r.items[0].status == denied {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the revocation never reached the daemon"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_never_serves_on_another_principals_decision() {
    run_daemon_decisions(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_daemon_never_serves_on_another_principals_decision() {
    if skip_external() {
        return;
    }
    run_daemon_decisions(Backend::External).await;
}
