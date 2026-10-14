//! SPEC 7.4 `Log` / `Blame` over `InProcessServer` (P24): success and paging, NOT_FOUND,
//! INVALID_ARGUMENT, PERMISSION_DENIED for restricted paths, `Log` without a path omitting
//! unreadable delta entries (19.12), redacted Meta rendered as `redacted = true` (19.13),
//! UNAUTHENTICATED calls, the three trailers on every response, and a blame version bump that
//! re-derives through the unchanged RPC (19.6).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use scion_core::{ChangeId, ObjectId, Principal, Tier, subcode};
use scion_derived::blame::{self, BlameType};
use scion_derived::{Registry, history};
use scion_history::{AllowAll, CreateRequest, MetaFields};
use scion_object::{Canonical, Changeset, DeltaEntry, DeltaOp, Extensions, RefName, RepoPath};
use scion_proto::{ScionClient, read_epochs, v1};
use scion_server::testing::{InProcessServer, ServerTestOpts};
use scion_store::{MemOptions, Storage};
use std::sync::Arc;

const ACL: &str = r#"version = 1

[groups]
eng = ["github:alice", "github:bob", "group:admins"]
admins = ["github:admin"]

[[paths]]
prefix = "/"
read = ["group:eng"]
write = ["group:eng"]

[[paths]]
prefix = "/secret"
read = ["group:admins"]
write = ["group:admins"]

[refs."*"]
update = ["group:eng"]
force = ["group:admins"]

[admin]
redact = ["group:admins"]
purge = ["group:admins"]
refs = ["group:admins"]
"#;

fn gh(login: &str) -> Principal {
    Principal::GitHub {
        login: login.to_owned(),
        id: Some(1),
    }
}

fn alice() -> Principal {
    gh("alice")
}

fn admin() -> Principal {
    gh("admin")
}

fn mallory() -> Principal {
    gh("mallory")
}

fn opts() -> ServerTestOpts {
    ServerTestOpts {
        acl_toml: ACL.to_owned(),
        ..ServerTestOpts::default()
    }
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s.as_bytes()).unwrap()
}

fn idb(id: &ObjectId) -> Vec<u8> {
    id.as_bytes().to_vec()
}

/// The seeded history:
/// - `root` (bootstrap, main@1)
/// - `c1`: put pub/a.txt "a\nb\n", secret/key "s1\n"
/// - `c2`: pub/a.txt "a\nB\nc\n"
/// - `c3`: secret/key "s2\n"
/// - `c4`: pub/a.txt "a\nB\nc\nd\n", secret/key "s3\n"
struct Seeded {
    root: ObjectId,
    c: Vec<ObjectId>,
}

async fn put(srv: &InProcessServer, content: &str) -> DeltaEntry {
    let (id, _) = srv
        .ctx()
        .objects
        .put_blob(Bytes::from(content.as_bytes().to_vec()))
        .await
        .unwrap();
    DeltaEntry {
        path: p("x"),
        op: DeltaOp::Put {
            mode: scion_core::Mode::File,
            id,
            size: content.len() as u64,
        },
    }
}

async fn entry(srv: &InProcessServer, path: &str, content: &str) -> DeltaEntry {
    let mut e = put(srv, content).await;
    e.path = p(path);
    e
}

fn fields(msg: &str) -> MetaFields {
    MetaFields {
        author: b"Alice <alice@example.com>".to_vec(),
        author_time: scion_object::Timestamp {
            seconds: 1_700_000_000,
            tz_offset_minutes: 60,
        },
        committer: None,
        committer_time: None,
        message: msg.as_bytes().to_vec(),
        extensions: Extensions::new(),
    }
}

async fn create(
    srv: &InProcessServer,
    parent: ObjectId,
    delta: Vec<DeltaEntry>,
    seq: u8,
) -> ObjectId {
    let req = CreateRequest {
        parents: vec![parent],
        delta,
        meta: fields(&format!("commit {seq}")),
        change_id: ChangeId([seq; 16]),
        expected_root_tree: None,
        import_mode: false,
    };
    srv.ctx()
        .history
        .create_changeset(req, &admin(), &AllowAll)
        .await
        .unwrap()
        .changeset_id
}

async fn seed(srv: &InProcessServer) -> Seeded {
    let ctx = srv.ctx();
    let root = ctx
        .history
        .resolve_ref(&RefName::parse("main").unwrap(), None)
        .await
        .unwrap()
        .target;
    let d1 = vec![
        entry(srv, "pub/a.txt", "a\nb\n").await,
        entry(srv, "secret/key", "s1\n").await,
    ];
    let c1 = create(srv, root, d1, 1).await;
    let c2 = create(srv, c1, vec![entry(srv, "pub/a.txt", "a\nB\nc\n").await], 2).await;
    let c3 = create(srv, c2, vec![entry(srv, "secret/key", "s2\n").await], 3).await;
    let d4 = vec![
        entry(srv, "pub/a.txt", "a\nB\nc\nd\n").await,
        entry(srv, "secret/key", "s3\n").await,
    ];
    let c4 = create(srv, c3, d4, 4).await;
    Seeded {
        root,
        c: vec![c1, c2, c3, c4],
    }
}

fn log_req(from: &ObjectId, path: Option<&str>, limit: u32, token: Vec<u8>) -> v1::LogRequest {
    v1::LogRequest {
        from: idb(from),
        path: path.map(|s| s.as_bytes().to_vec()),
        first_parent_only: false,
        limit,
        page_token: token,
    }
}

fn entry_ids(r: &v1::LogResponse) -> Vec<ObjectId> {
    r.entries
        .iter()
        .map(|e| ObjectId::from_bytes(&e.id).unwrap())
        .collect()
}

fn assert_trailers(md: &tonic::metadata::MetadataMap) {
    let e = read_epochs(&md.clone().into_headers()).expect("epoch trailers present");
    assert_eq!(e.tier, Tier::Server);
}

async fn log(c: &ScionClient, req: v1::LogRequest) -> Result<v1::LogResponse, tonic::Status> {
    match c.query().log(req).await {
        Ok(r) => {
            assert_trailers(r.metadata());
            Ok(r.into_inner())
        }
        Err(s) => {
            assert_trailers(s.metadata());
            Err(s)
        }
    }
}

async fn blame_rpc(
    c: &ScionClient,
    cs: &ObjectId,
    path: &str,
) -> Result<v1::BlameResponse, tonic::Status> {
    let req = v1::BlameRequest {
        changeset: idb(cs),
        path: path.as_bytes().to_vec(),
    };
    match c.query().blame(req).await {
        Ok(r) => {
            assert_trailers(r.metadata());
            Ok(r.into_inner())
        }
        Err(s) => {
            assert_trailers(s.metadata());
            Err(s)
        }
    }
}

fn code_of(s: &tonic::Status) -> scion_core::ErrorCode {
    scion_proto::error_from_status(s).code
}

/// Every delta path of an entry's view (encoded or filtered).
fn delta_paths(e: &v1::LogEntry) -> (Vec<Vec<u8>>, bool) {
    let item = e.changeset.as_ref().unwrap();
    assert_eq!(item.status, v1::ItemStatus::Ok as i32);
    assert_eq!(item.id, e.id);
    match item.view.as_ref().unwrap() {
        v1::changeset_item::View::Encoded(b) => {
            let cs = Changeset::decode(b).unwrap();
            assert_eq!(cs.id().as_bytes().to_vec(), e.id, "encoding matches the id");
            (
                cs.delta
                    .iter()
                    .map(|d| d.path.as_bytes().to_vec())
                    .collect(),
                false,
            )
        }
        v1::changeset_item::View::FilteredView(f) => {
            assert!(f.filtered);
            (f.delta.iter().map(|d| d.path.clone()).collect(), true)
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn log_without_path_pages_and_renders_meta() {
    let srv = InProcessServer::start(opts()).await;
    let s = seed(&srv).await;
    let c = srv.client(Some(&admin())).await;
    let all = log(&c, log_req(&s.c[3], None, 0, vec![])).await.unwrap();
    let mut want = s.c.clone();
    want.reverse();
    want.push(s.root);
    assert_eq!(entry_ids(&all), want);
    assert!(all.next_page_token.is_empty());
    // Admin reads every path: full encodings, generations, Meta.
    for (i, e) in all.entries.iter().enumerate() {
        let (_, filtered) = delta_paths(e);
        assert!(!filtered);
        let item = e.changeset.as_ref().unwrap();
        let id = ObjectId::from_bytes(&e.id).unwrap();
        let rec = srv.ctx().history.changeset(&id).await.unwrap();
        assert_eq!(item.generation, rec.generation);
        let m = e.meta.as_ref().unwrap();
        assert!(!m.redacted);
        assert_eq!(m.meta_id, idb(&rec.changeset.meta));
        if i < 4 {
            assert_eq!(m.author, b"Alice <alice@example.com>".to_vec());
            assert_eq!(m.message, format!("commit {}", 4 - i).into_bytes());
            assert_eq!(m.creator, b"github:admin".to_vec());
            assert_eq!(m.author_time.as_ref().unwrap().tz_offset_minutes, 60);
        }
    }
    // Pages of 2 concatenate to the full log.
    let mut got = Vec::new();
    let mut token = Vec::new();
    loop {
        let page = log(&c, log_req(&s.c[3], None, 2, token)).await.unwrap();
        assert!(page.entries.len() <= 2);
        got.extend(entry_ids(&page));
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    assert_eq!(got, want);
    // first_parent_only on a linear history is the same.
    let mut req = log_req(&s.c[3], None, 0, vec![]);
    req.first_parent_only = true;
    assert_eq!(entry_ids(&log(&c, req).await.unwrap()), want);
}

#[tokio::test(flavor = "multi_thread")]
async fn log_without_path_omits_unreadable_delta_entries() {
    let srv = InProcessServer::start(opts()).await;
    let s = seed(&srv).await;
    let c = srv.client(Some(&alice())).await;
    let all = log(&c, log_req(&s.c[3], None, 0, vec![])).await.unwrap();
    assert_eq!(all.entries.len(), 5);
    for e in &all.entries {
        let (paths, _) = delta_paths(e);
        assert!(
            paths.iter().all(|p| !p.starts_with(b"secret")),
            "no restricted path is returned"
        );
    }
    let by_id = |id: &ObjectId| {
        all.entries
            .iter()
            .find(|e| e.id == idb(id))
            .cloned()
            .unwrap()
    };
    // c1 and c4 touch both trees: filtered, keeping pub/a.txt. c3 touches only secret/key:
    // filtered to an empty delta. c2 is readable: the full encoding.
    for id in [s.c[0], s.c[3]] {
        let (paths, filtered) = delta_paths(&by_id(&id));
        assert!(filtered);
        assert_eq!(paths, vec![b"pub/a.txt".to_vec()]);
    }
    let (paths, filtered) = delta_paths(&by_id(&s.c[2]));
    assert!(filtered);
    assert!(paths.is_empty());
    let (_, filtered) = delta_paths(&by_id(&s.c[1]));
    assert!(!filtered);
    // The filtered view keeps the header fields.
    let e4 = by_id(&s.c[3]);
    let Some(v1::changeset_item::View::FilteredView(f)) =
        e4.changeset.as_ref().unwrap().view.clone()
    else {
        panic!("filtered view expected");
    };
    let rec = srv.ctx().history.changeset(&s.c[3]).await.unwrap();
    assert_eq!(f.parents, vec![idb(&s.c[2])]);
    assert_eq!(f.root_tree, idb(&rec.changeset.root_tree));
    assert_eq!(f.meta, idb(&rec.changeset.meta));
    assert_eq!(f.change_id, vec![4u8; 16]);
}

#[tokio::test(flavor = "multi_thread")]
async fn log_with_path_follows_the_path_and_checks_read() {
    let srv = InProcessServer::start(opts()).await;
    let s = seed(&srv).await;
    let a = srv.client(Some(&alice())).await;
    let r = log(&a, log_req(&s.c[3], Some("pub/a.txt"), 0, vec![]))
        .await
        .unwrap();
    assert_eq!(entry_ids(&r), vec![s.c[3], s.c[1], s.c[0]]);
    for e in &r.entries {
        let (paths, _) = delta_paths(e);
        assert!(paths.iter().all(|p| !p.starts_with(b"secret")));
    }
    // Directory paths and paging.
    let first = log(&a, log_req(&s.c[3], Some("pub"), 1, vec![]))
        .await
        .unwrap();
    assert_eq!(entry_ids(&first), vec![s.c[3]]);
    assert!(!first.next_page_token.is_empty());
    let rest = log(
        &a,
        log_req(&s.c[3], Some("pub"), 10, first.next_page_token.clone()),
    )
    .await
    .unwrap();
    assert_eq!(entry_ids(&rest), vec![s.c[1], s.c[0]]);
    // A token of another query is INVALID_ARGUMENT / bad_page_token.
    let err = log(
        &a,
        log_req(&s.c[3], Some("pub/a.txt"), 10, first.next_page_token),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        scion_proto::error_from_status(&err).subcode(),
        Some(subcode::BAD_PAGE_TOKEN)
    );
    // A path absent at `from` has an empty log.
    let none = log(&a, log_req(&s.c[3], Some("pub/nope"), 0, vec![]))
        .await
        .unwrap();
    assert!(none.entries.is_empty());

    // Restricted path: PERMISSION_DENIED for alice, existing or not; admin sees its history.
    for path in ["secret/key", "secret/missing", "secret"] {
        let err = log(&a, log_req(&s.c[3], Some(path), 0, vec![]))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied, "{path}");
    }
    let adm = srv.client(Some(&admin())).await;
    let r = log(&adm, log_req(&s.c[3], Some("secret/key"), 0, vec![]))
        .await
        .unwrap();
    assert_eq!(entry_ids(&r), vec![s.c[3], s.c[2], s.c[0]]);

    // A principal without repository read is denied with and without a path.
    let m = srv.client(Some(&mallory())).await;
    for path in [None, Some("pub/a.txt"), Some("")] {
        let err = log(&m, log_req(&s.c[3], path, 0, vec![]))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied, "{path:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn log_errors() {
    let srv = InProcessServer::start(opts()).await;
    let s = seed(&srv).await;
    let a = srv.client(Some(&alice())).await;
    let unknown = ObjectId::new(scion_core::Kind::Changeset, [9; 32]);
    for path in [None, Some("pub/a.txt")] {
        let err = log(&a, log_req(&unknown, path, 0, vec![]))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound, "{path:?}");
        assert_eq!(code_of(&err), scion_core::ErrorCode::NotFound);
    }
    // Missing and malformed `from`.
    let mut req = log_req(&s.c[0], None, 0, vec![]);
    req.from = vec![];
    let err = log(&a, req).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        scion_proto::error_from_status(&err).subcode(),
        Some(subcode::MISSING_FIELD)
    );
    let mut req = log_req(&s.c[0], None, 0, vec![]);
    req.from = vec![1, 2, 3];
    let err = log(&a, req).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    // Malformed path and page token.
    let mut req = log_req(&s.c[0], None, 0, vec![]);
    req.path = Some(b"/abs".to_vec());
    assert_eq!(
        log(&a, req).await.unwrap_err().code(),
        tonic::Code::InvalidArgument
    );
    let err = log(&a, log_req(&s.c[0], None, 0, b"junk".to_vec()))
        .await
        .unwrap_err();
    assert_eq!(
        scion_proto::error_from_status(&err).subcode(),
        Some(subcode::BAD_PAGE_TOKEN)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn blame_success_errors_and_read_checks() {
    let srv = InProcessServer::start(opts()).await;
    let s = seed(&srv).await;
    let a = srv.client(Some(&alice())).await;
    let r = blame_rpc(&a, &s.c[3], "pub/a.txt").await.unwrap();
    let got: Vec<(u32, u32, Vec<u8>, u32)> = r
        .ranges
        .iter()
        .map(|x| {
            (
                x.start_line,
                x.line_count,
                x.origin_changeset.clone(),
                x.origin_start_line,
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (0, 1, idb(&s.c[0]), 0),
            (1, 2, idb(&s.c[1]), 1),
            (3, 1, idb(&s.c[3]), 3),
        ]
    );
    // Equal to the library result.
    let lib = blame::blame(&srv.ctx().derived, &s.c[3], &p("pub/a.txt"), None)
        .await
        .unwrap();
    assert_eq!(lib.len(), r.ranges.len());

    // NOT_FOUND: unknown changeset, missing path, directory.
    let unknown = ObjectId::new(scion_core::Kind::Changeset, [9; 32]);
    let err = blame_rpc(&a, &unknown, "pub/a.txt").await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
    for path in ["pub/missing", "pub"] {
        let err = blame_rpc(&a, &s.c[3], path).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound, "{path}");
    }
    // INVALID_ARGUMENT: missing path / changeset, malformed path.
    let err = blame_rpc(&a, &s.c[3], "").await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        scion_proto::error_from_status(&err).subcode(),
        Some(subcode::MISSING_FIELD)
    );
    let err = a
        .query()
        .blame(v1::BlameRequest {
            changeset: vec![],
            path: b"pub/a.txt".to_vec(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    let err = blame_rpc(&a, &s.c[3], "pub//a").await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    // PERMISSION_DENIED for restricted paths, whether or not they exist (no probing).
    for path in ["secret/key", "secret/nope"] {
        let err = blame_rpc(&a, &s.c[3], path).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied, "{path}");
    }
    let m = srv.client(Some(&mallory())).await;
    let err = blame_rpc(&m, &s.c[3], "pub/a.txt").await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    // Admin may blame the restricted file.
    let adm = srv.client(Some(&admin())).await;
    let r = blame_rpc(&adm, &s.c[3], "secret/key").await.unwrap();
    assert_eq!(r.ranges.len(), 1);
    assert_eq!(r.ranges[0].origin_changeset, idb(&s.c[3]));
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthenticated_calls_fail() {
    let srv = InProcessServer::start(opts()).await;
    let s = seed(&srv).await;
    let c = srv.client(None).await;
    let err = log(&c, log_req(&s.c[3], None, 0, vec![]))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    let err = blame_rpc(&c, &s.c[3], "pub/a.txt").await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    // An invalid token too.
    let bad = c.with_token(Some("v4.public.garbage".to_owned()));
    let err = log(&bad, log_req(&s.c[3], None, 0, vec![]))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test(flavor = "multi_thread")]
async fn redacted_meta_is_rendered_redacted_with_ids_unchanged() {
    let srv = InProcessServer::start(opts()).await;
    let s = seed(&srv).await;
    let ctx = srv.ctx();
    let c = srv.client(Some(&alice())).await;
    let before = log(&c, log_req(&s.c[3], None, 0, vec![])).await.unwrap();
    let meta2 = ctx.history.changeset(&s.c[1]).await.unwrap().changeset.meta;
    ctx.redactor
        .redact(&[meta2], "leaked secret in message", &admin(), None)
        .await
        .unwrap();
    ctx.redactions.refresh().await.unwrap();
    let after = log(&c, log_req(&s.c[3], None, 0, vec![])).await.unwrap();
    assert_eq!(entry_ids(&before), entry_ids(&after), "ids unchanged");
    for (b, a) in before.entries.iter().zip(&after.entries) {
        assert_eq!(b.changeset, a.changeset, "changeset items unchanged");
        let m = a.meta.as_ref().unwrap();
        if a.id == idb(&s.c[1]) {
            assert_eq!(
                *m,
                v1::MetaView {
                    redacted: true,
                    meta_id: idb(&meta2),
                    ..v1::MetaView::default()
                }
            );
        } else {
            assert!(!m.redacted);
            assert_eq!(m, b.meta.as_ref().unwrap());
        }
    }
    // Through Log(path) too, and after purge.
    ctx.redactor
        .purge_now(&[meta2], &admin(), "purge")
        .await
        .unwrap();
    let r = log(&c, log_req(&s.c[3], Some("pub/a.txt"), 0, vec![]))
        .await
        .unwrap();
    let e2 = r.entries.iter().find(|e| e.id == idb(&s.c[1])).unwrap();
    assert!(e2.meta.as_ref().unwrap().redacted);
    assert!(e2.meta.as_ref().unwrap().message.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn blame_version_bump_rederives_through_the_unchanged_rpc() {
    let (storage, _mem) = Storage::in_memory(MemOptions::default());
    let mut first = InProcessServer::start(ServerTestOpts {
        storage: Some(storage.clone()),
        ..opts()
    })
    .await;
    let s = seed(&first).await;
    let a = first.client(Some(&alice())).await;
    let v1_resp = blame_rpc(&a, &s.c[3], "pub/a.txt").await.unwrap();
    let ctx = first.ctx();
    let (node, _) = history::node_for_path(ctx.derived.ctx(), &s.c[3], &p("pub/a.txt"))
        .await
        .unwrap()
        .unwrap();
    let v1_bytes = ctx
        .derived
        .get_stored(blame::NAME, Some(1), node.as_bytes())
        .await
        .unwrap()
        .expect("v1 stored");
    drop(ctx);
    first.kill().await;

    // A new server over the same storage with blame v2 registered.
    let second = InProcessServer::start(ServerTestOpts {
        storage: Some(storage.clone()),
        registry: Some(Arc::new(|| {
            let mut r = Registry::v1_default();
            r.register_lazy(Arc::new(BlameType::with_version(2)));
            r
        })),
        ..opts()
    })
    .await;
    let ctx = second.ctx();
    assert!(
        ctx.derived
            .get_stored(blame::NAME, Some(2), node.as_bytes())
            .await
            .unwrap()
            .is_none()
    );
    let a = second.client(Some(&alice())).await;
    let v2_resp = blame_rpc(&a, &s.c[3], "pub/a.txt").await.unwrap();
    assert_eq!(v2_resp, v1_resp);
    assert!(
        ctx.derived
            .get_stored(blame::NAME, Some(2), node.as_bytes())
            .await
            .unwrap()
            .is_some(),
        "the unchanged RPC re-derived blame at v2"
    );
    assert_eq!(
        ctx.derived
            .get(blame::NAME, Some(1), node.as_bytes())
            .await
            .unwrap(),
        v1_bytes,
        "v1 values stay readable"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn log_of_a_readable_directory_never_returns_restricted_entries() {
    // SPEC 12.6 / 19.12: Log(path) of a readable directory that contains a restricted subtree
    // (here the root, containing /secret) lists the directory's history, but every entry's
    // delta omits the restricted paths; first_parent_only and paging change nothing.
    let srv = InProcessServer::start(opts()).await;
    let s = seed(&srv).await;
    let a = srv.client(Some(&alice())).await;
    let mut want = s.c.clone();
    want.reverse();
    want.push(s.root);
    for fp in [false, true] {
        let mut got = Vec::new();
        let mut token = Vec::new();
        loop {
            let mut req = log_req(&s.c[3], Some(""), 2, token);
            req.first_parent_only = fp;
            let page = log(&a, req).await.unwrap();
            for e in &page.entries {
                let (paths, _) = delta_paths(e);
                assert!(
                    paths.iter().all(|p| !p.starts_with(b"secret")),
                    "no restricted path is returned"
                );
            }
            got.extend(entry_ids(&page));
            if page.next_page_token.is_empty() {
                break;
            }
            token = page.next_page_token;
        }
        assert_eq!(got, want, "first_parent_only {fp}");
    }
    // The admin sees the same changesets with full encodings.
    let adm = srv.client(Some(&admin())).await;
    let r = log(&adm, log_req(&s.c[3], Some(""), 0, vec![]))
        .await
        .unwrap();
    assert_eq!(entry_ids(&r), want);
    assert!(r.entries.iter().all(|e| !delta_paths(e).1));
}
