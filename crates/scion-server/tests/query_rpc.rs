//! SPEC 19.7 per-RPC contracts of the query service (success, NOT_FOUND, INVALID_ARGUMENT,
//! PERMISSION_DENIED, UNAUTHENTICATED, trailers on the streaming RPCs), Diff content hunks, and
//! SPEC 19.6 the generic derived-data path through GetDerived (P27).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod query_support;

use async_trait::async_trait;
use bytes::Bytes;
use query_support::*;
use scion_core::{ErrorCode, Kind, ObjectId, Principal, Result, subcode};
use scion_derived::{DeriveCtx, DeriveMode, DerivedType, LazyType, Registry};
use scion_proto::v1;
use scion_proto::v1::query_service_client::QueryServiceClient;
use scion_server::testing::{InProcessServer, ServerTestOpts};
use std::sync::Arc;

type Q = QueryServiceClient<scion_proto::Transport>;

async fn q(srv: &InProcessServer, who: Option<&Principal>) -> Q {
    srv.client(who).await.query()
}

fn missing_cs() -> ObjectId {
    ObjectId::new(Kind::Changeset, [0x5a; 32])
}

/// Drain a server stream and check its trailers carry the three SPEC 7.1 values.
async fn drain<T>(mut s: tonic::Streaming<T>) -> Vec<T> {
    let mut out = Vec::new();
    while let Some(m) = s.message().await.unwrap() {
        out.push(m);
    }
    let t = s.trailers().await.unwrap().expect("stream trailers");
    let e = epochs(&t).expect("epoch trailers on the stream");
    assert_eq!(e.tier, scion_core::Tier::Server);
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn get_changesets_contract() {
    let r = repo().await;
    let mut a = q(&r.srv, Some(&alice())).await;
    // Success with mixed per-item statuses; a non-changeset id is not_found too.
    let tree = r
        .ctx
        .history
        .changeset(&r.c3)
        .await
        .unwrap()
        .changeset
        .root_tree;
    let resp = a
        .get_changesets(v1::GetChangesetsRequest {
            ids: vec![idb(&r.c3), idb(&missing_cs()), idb(&tree), idb(&r.root)],
        })
        .await
        .unwrap();
    assert!(epochs(resp.metadata()).is_some());
    let items = resp.into_inner().items;
    let st: Vec<i32> = items.iter().map(|i| i.status).collect();
    let ok = v1::ItemStatus::Ok as i32;
    let nf = v1::ItemStatus::NotFound as i32;
    assert_eq!(st, vec![ok, nf, nf, ok]);
    assert_eq!(items[1].id, idb(&missing_cs()));
    assert!(items[1].view.is_none());
    assert_eq!(items[3].generation, 1);
    assert_eq!(items[0].generation, 4);
    // Empty batch.
    let resp = a
        .get_changesets(v1::GetChangesetsRequest { ids: vec![] })
        .await
        .unwrap();
    assert!(resp.into_inner().items.is_empty());
    // Malformed id.
    let e = a
        .get_changesets(v1::GetChangesetsRequest {
            ids: vec![vec![1, 2, 3]],
        })
        .await;
    assert_eq!(
        expect_code(e, ErrorCode::InvalidArgument).subcode(),
        Some(subcode::BAD_ID_TEXT)
    );
    let req = || v1::GetChangesetsRequest {
        ids: vec![idb(&r.c1)],
    };
    expect_code(
        q(&r.srv, Some(&mallory()))
            .await
            .get_changesets(req())
            .await,
        ErrorCode::PermissionDenied,
    );
    expect_code(
        q(&r.srv, None).await.get_changesets(req()).await,
        ErrorCode::Unauthenticated,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn read_path_contract() {
    let r = repo().await;
    let mut a = q(&r.srv, Some(&alice())).await;
    let req = |cs: &ObjectId, p: &str| v1::ReadPathRequest {
        changeset: idb(cs),
        path: p.as_bytes().to_vec(),
    };
    // A file: entry on the first chunk, content, final on the last; trailers on the stream.
    let chunks = drain(
        a.read_path(req(&r.c2, "README"))
            .await
            .unwrap()
            .into_inner(),
    )
    .await;
    let first = chunks.first().unwrap();
    let entry = first.entry.as_ref().unwrap();
    assert_eq!(entry.mode, 1);
    assert_eq!(entry.size, README_2.len() as u64);
    assert_eq!(entry.id, idb(&ObjectId::hash_blob(README_2)));
    assert!(chunks.last().unwrap().r#final);
    let data: Vec<u8> = chunks.iter().flat_map(|c| c.data.clone()).collect();
    assert_eq!(data, README_2);
    // A directory and the root: the entry only.
    for p in ["src", ""] {
        let chunks = drain(a.read_path(req(&r.c2, p)).await.unwrap().into_inner()).await;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].entry.as_ref().unwrap().mode, 4);
        assert!(chunks[0].data.is_empty() && chunks[0].r#final);
    }
    let root_tree = r
        .ctx
        .history
        .changeset(&r.c2)
        .await
        .unwrap()
        .changeset
        .root_tree;
    let chunks = drain(a.read_path(req(&r.c2, "")).await.unwrap().into_inner()).await;
    assert_eq!(chunks[0].entry.as_ref().unwrap().id, idb(&root_tree));
    // A large file spans several chunks.
    let big: Vec<u8> = (0..(3 << 20) + 17).map(|i| (i % 251) as u8).collect();
    let cb = commit(&r.ctx, &[r.c3], &[("big.bin", &big)], &[]).await;
    let chunks = drain(a.read_path(req(&cb, "big.bin")).await.unwrap().into_inner()).await;
    assert!(chunks.len() >= 4);
    assert!(chunks[1..].iter().all(|c| c.entry.is_none()));
    assert_eq!(
        chunks.iter().filter(|c| c.r#final).count(),
        1,
        "exactly one final chunk"
    );
    let data: Vec<u8> = chunks.iter().flat_map(|c| c.data.clone()).collect();
    assert_eq!(data, big);

    // NOT_FOUND: missing path, path below a file, missing changeset.
    for (cs, p) in [
        (r.c2, "nope"),
        (r.c2, "README/x"),
        (r.c2, "team/c.txt"),
        (missing_cs(), "README"),
    ] {
        expect_code(a.read_path(req(&cs, p)).await, ErrorCode::NotFound);
    }
    // INVALID_ARGUMENT: missing changeset, malformed path, non-changeset id.
    let e = a
        .read_path(v1::ReadPathRequest {
            changeset: vec![],
            path: b"README".to_vec(),
        })
        .await;
    assert_eq!(
        expect_code(e, ErrorCode::InvalidArgument).subcode(),
        Some(subcode::MISSING_FIELD)
    );
    for p in ["/README", "a//b", "README/", "x/../y"] {
        let e = expect_code(a.read_path(req(&r.c2, p)).await, ErrorCode::InvalidArgument);
        assert_eq!(e.subcode(), Some(subcode::INVALID_PATH), "{p}");
    }
    expect_code(
        a.read_path(req(&root_tree, "README")).await,
        ErrorCode::InvalidArgument,
    );
    // PERMISSION_DENIED and UNAUTHENTICATED (the error responses carry trailers too).
    expect_code(
        q(&r.srv, Some(&mallory()))
            .await
            .read_path(req(&r.c2, "README"))
            .await,
        ErrorCode::PermissionDenied,
    );
    expect_code(
        a.read_path(req(&r.c2, "secret/key.txt")).await,
        ErrorCode::PermissionDenied,
    );
    expect_code(
        q(&r.srv, None).await.read_path(req(&r.c2, "README")).await,
        ErrorCode::Unauthenticated,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_tree_contract() {
    let r = repo().await;
    let mut a = q(&r.srv, Some(&alice())).await;
    let req = |cs: &ObjectId, p: &str| v1::ListTreeRequest {
        changeset: idb(cs),
        path: p.as_bytes().to_vec(),
    };
    let src = a.list_tree(req(&r.c2, "src")).await.unwrap();
    assert!(epochs(src.metadata()).is_some());
    let src = src.into_inner();
    assert_eq!(src.entries.len(), 1);
    assert_eq!(src.entries[0].name, b"main.rs");
    let e = src.entries[0].entry.as_ref().unwrap();
    assert_eq!((e.mode, e.size), (1, b"fn main() {}\n".len() as u64));
    // The tree id is the id of the listed tree.
    let root_tree = r
        .ctx
        .history
        .changeset(&r.c2)
        .await
        .unwrap()
        .changeset
        .root_tree;
    let want = r
        .ctx
        .history
        .lookup(&root_tree, &path("src"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(src.tree_id, idb(&want.id));
    // The root of the root changeset is the empty tree.
    let empty = a.list_tree(req(&r.root, "")).await.unwrap().into_inner();
    assert!(empty.entries.is_empty());
    assert_eq!(empty.tree_id, idb(&scion_core::empty_tree_id()));

    for (cs, p) in [(r.c2, "nope"), (r.c1, "team/b.txt"), (missing_cs(), "")] {
        expect_code(a.list_tree(req(&cs, p)).await, ErrorCode::NotFound);
    }
    let e = expect_code(
        a.list_tree(req(&r.c2, "README")).await,
        ErrorCode::InvalidArgument,
    );
    assert_eq!(e.subcode(), Some(subcode::INVALID_PATH));
    expect_code(
        a.list_tree(req(&r.c2, "/src")).await,
        ErrorCode::InvalidArgument,
    );
    let e = a
        .list_tree(v1::ListTreeRequest {
            changeset: vec![],
            path: vec![],
        })
        .await;
    assert_eq!(
        expect_code(e, ErrorCode::InvalidArgument).subcode(),
        Some(subcode::MISSING_FIELD)
    );
    expect_code(
        q(&r.srv, Some(&mallory()))
            .await
            .list_tree(req(&r.c2, ""))
            .await,
        ErrorCode::PermissionDenied,
    );
    expect_code(
        q(&r.srv, None).await.list_tree(req(&r.c2, "")).await,
        ErrorCode::Unauthenticated,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_contract_and_hunks() {
    let r = repo().await;
    let mut a = q(&r.srv, Some(&alice())).await;
    let req = |x: &ObjectId, y: &ObjectId, hunks: bool| v1::DiffRequest {
        a: idb(x),
        b: idb(y),
        paths: vec![],
        include_content_hunks: hunks,
    };
    let items = drain(a.diff(req(&r.c1, &r.c2, true)).await.unwrap().into_inner()).await;
    let paths: Vec<&[u8]> = items.iter().map(|i| i.path.as_slice()).collect();
    assert_eq!(paths, vec![b"README".as_slice(), b"bin.dat", b"team/b.txt"]);
    // README: one text hunk.
    let readme = &items[0];
    assert!(!readme.binary);
    assert_eq!(
        readme.old_entry.as_ref().unwrap().id,
        idb(&ObjectId::hash_blob(README_1))
    );
    assert_eq!(
        readme.new_entry.as_ref().unwrap().id,
        idb(&ObjectId::hash_blob(README_2))
    );
    assert_eq!(readme.hunks.len(), 1);
    let h = &readme.hunks[0];
    assert_eq!(
        (h.old_start, h.old_lines, h.new_start, h.new_lines),
        (1, 1, 1, 2)
    );
    assert_eq!(h.lines, vec![b" hello\n".to_vec(), b"+world\n".to_vec()]);
    // bin.dat: binary, no hunks.
    assert!(items[1].binary && items[1].hunks.is_empty());
    // team/b.txt: added.
    assert!(items[2].old_entry.is_none());
    assert_eq!(items[2].hunks[0].lines, vec![b"+b\n".to_vec()]);
    // Without hunks requested: entries only.
    let items = drain(a.diff(req(&r.c1, &r.c2, false)).await.unwrap().into_inner()).await;
    assert!(items.iter().all(|i| i.hunks.is_empty() && !i.binary));
    // Scoped, and identical changesets.
    let items = drain(
        a.diff(v1::DiffRequest {
            paths: vec![b"team".to_vec()],
            ..req(&r.c1, &r.c2, false)
        })
        .await
        .unwrap()
        .into_inner(),
    )
    .await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].path, b"team/b.txt");
    assert!(
        drain(a.diff(req(&r.c2, &r.c2, true)).await.unwrap().into_inner())
            .await
            .is_empty()
    );
    // Reverse direction: deletion of team/b.txt.
    let items = drain(a.diff(req(&r.c2, &r.c1, false)).await.unwrap().into_inner()).await;
    let b = items.iter().find(|i| i.path == b"team/b.txt").unwrap();
    assert!(b.new_entry.is_none() && b.old_entry.is_some());

    expect_code(
        a.diff(req(&r.c1, &missing_cs(), false)).await,
        ErrorCode::NotFound,
    );
    expect_code(
        a.diff(req(&missing_cs(), &r.c1, false)).await,
        ErrorCode::NotFound,
    );
    let e = a
        .diff(v1::DiffRequest {
            b: vec![],
            ..req(&r.c1, &r.c2, false)
        })
        .await;
    assert_eq!(
        expect_code(e, ErrorCode::InvalidArgument).subcode(),
        Some(subcode::MISSING_FIELD)
    );
    let e = a
        .diff(v1::DiffRequest {
            paths: vec![b"/bad/".to_vec()],
            ..req(&r.c1, &r.c2, false)
        })
        .await;
    expect_code(e, ErrorCode::InvalidArgument);
    let tree = r
        .ctx
        .history
        .changeset(&r.c1)
        .await
        .unwrap()
        .changeset
        .root_tree;
    expect_code(
        a.diff(req(&r.c1, &tree, false)).await,
        ErrorCode::InvalidArgument,
    );
    expect_code(
        q(&r.srv, Some(&mallory()))
            .await
            .diff(req(&r.c1, &r.c2, false))
            .await,
        ErrorCode::PermissionDenied,
    );
    expect_code(
        q(&r.srv, None).await.diff(req(&r.c1, &r.c2, false)).await,
        ErrorCode::Unauthenticated,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn is_ancestor_and_merge_base_contract() {
    let r = repo().await;
    let mut a = q(&r.srv, Some(&alice())).await;
    let side = commit(&r.ctx, &[r.c1], &[("side.txt", b"s\n")], &[]).await;
    for (x, y, want) in [
        (r.c1, r.c3, true),
        (r.c3, r.c1, false),
        (r.c2, r.c2, true),
        (r.root, side, true),
        (side, r.c3, false),
    ] {
        let resp = a
            .is_ancestor(v1::IsAncestorRequest {
                a: idb(&x),
                b: idb(&y),
            })
            .await
            .unwrap();
        assert!(epochs(resp.metadata()).is_some());
        assert_eq!(resp.into_inner().result, want);
    }
    let bases = a
        .merge_base(v1::MergeBaseRequest {
            a: idb(&side),
            b: idb(&r.c3),
        })
        .await
        .unwrap()
        .into_inner()
        .bases;
    assert_eq!(bases, vec![idb(&r.c1)]);
    // Unrelated histories (an imported root) have no merge base.
    let imported = import_commit(&r.ctx, [7; 20], &[("x", b"x\n")]).await;
    let bases = a
        .merge_base(v1::MergeBaseRequest {
            a: idb(&imported),
            b: idb(&r.c3),
        })
        .await
        .unwrap()
        .into_inner()
        .bases;
    assert!(bases.is_empty());

    let ia = |x: Vec<u8>, y: Vec<u8>| v1::IsAncestorRequest { a: x, b: y };
    let mb = |x: Vec<u8>, y: Vec<u8>| v1::MergeBaseRequest { a: x, b: y };
    expect_code(
        a.is_ancestor(ia(idb(&r.c1), idb(&missing_cs()))).await,
        ErrorCode::NotFound,
    );
    expect_code(
        a.merge_base(mb(idb(&missing_cs()), idb(&r.c1))).await,
        ErrorCode::NotFound,
    );
    let e = a.is_ancestor(ia(vec![], idb(&r.c1))).await;
    assert_eq!(
        expect_code(e, ErrorCode::InvalidArgument).subcode(),
        Some(subcode::MISSING_FIELD)
    );
    let e = a.merge_base(mb(idb(&r.c1), vec![9; 5])).await;
    assert_eq!(
        expect_code(e, ErrorCode::InvalidArgument).subcode(),
        Some(subcode::BAD_ID_TEXT)
    );
    let mut m = q(&r.srv, Some(&mallory())).await;
    expect_code(
        m.is_ancestor(ia(idb(&r.c1), idb(&r.c2))).await,
        ErrorCode::PermissionDenied,
    );
    expect_code(
        m.merge_base(mb(idb(&r.c1), idb(&r.c2))).await,
        ErrorCode::PermissionDenied,
    );
    let mut n = q(&r.srv, None).await;
    expect_code(
        n.is_ancestor(ia(idb(&r.c1), idb(&r.c2))).await,
        ErrorCode::Unauthenticated,
    );
    expect_code(
        n.merge_base(mb(idb(&r.c1), idb(&r.c2))).await,
        ErrorCode::Unauthenticated,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_contract() {
    let r = repo().await;
    let mut a = q(&r.srv, Some(&alice())).await;
    let req = |s: &str| v1::ResolveRequest {
        revision: s.to_owned(),
    };
    let resp = a.resolve(req("main")).await.unwrap();
    assert!(epochs(resp.metadata()).is_some());
    assert_eq!(resp.into_inner().changeset, idb(&r.root));
    for s in ["nope", "main@9", "deadbeefdeadbeef"] {
        expect_code(a.resolve(req(s)).await, ErrorCode::NotFound);
    }
    expect_code(
        a.resolve(req(&missing_cs().to_text())).await,
        ErrorCode::NotFound,
    );
    let e = expect_code(a.resolve(req("")).await, ErrorCode::InvalidArgument);
    assert_eq!(e.subcode(), Some(subcode::MISSING_FIELD));
    for s in ["Main", "main~x", "main^0", "git:abc", "a..b", "main@"] {
        let e = expect_code(a.resolve(req(s)).await, ErrorCode::InvalidArgument);
        assert_eq!(e.subcode(), Some(subcode::INVALID_REVISION), "{s}");
    }
    expect_code(
        q(&r.srv, Some(&mallory())).await.resolve(req("main")).await,
        ErrorCode::PermissionDenied,
    );
    expect_code(
        q(&r.srv, None).await.resolve(req("main")).await,
        ErrorCode::Unauthenticated,
    );
}

// ---------------------------------------------------------------------------------------------
// SPEC 19.6: a test-only derived type through the generic path
// ---------------------------------------------------------------------------------------------

/// A lazy test type: `value = "v<version>:" + hex(key)`, independent of other keys.
struct EchoType {
    version: u32,
}

impl DerivedType for EchoType {
    fn name(&self) -> &'static str {
        "test_echo"
    }
    fn version(&self) -> u32 {
        self.version
    }
    fn mode(&self) -> DeriveMode {
        DeriveMode::Lazy
    }
    fn content_bearing(&self) -> bool {
        false
    }
}

#[async_trait]
impl LazyType for EchoType {
    async fn deps(&self, _cx: &dyn DeriveCtx, _key: &[u8]) -> Result<Vec<Bytes>> {
        Ok(Vec::new())
    }
    async fn derive(
        &self,
        _cx: &dyn DeriveCtx,
        key: &[u8],
        _deps: &[(Bytes, Bytes)],
    ) -> Result<Bytes> {
        Ok(Bytes::from(format!(
            "v{}:{}",
            self.version,
            hex::encode(key)
        )))
    }
}

fn registry_with(versions: &'static [u32]) -> Arc<dyn Fn() -> Registry + Send + Sync> {
    Arc::new(move || {
        let mut r = Registry::v1_default();
        for v in versions {
            r.register_lazy(Arc::new(EchoType { version: *v }));
        }
        r
    })
}

fn derived_req(ty: &str, version: Option<u32>, key: &[u8]) -> v1::GetDerivedRequest {
    v1::GetDerivedRequest {
        r#type: ty.to_owned(),
        version,
        key: key.to_vec(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_derived_serves_a_test_only_type_and_old_versions() {
    // Server A knows only v1 of the test type.
    let a_srv = InProcessServer::start(ServerTestOpts {
        registry: Some(registry_with(&[1])),
        ..opts()
    })
    .await;
    let mut a = q(&a_srv, Some(&alice())).await;
    let resp = a
        .get_derived(derived_req("test_echo", None, b"k1"))
        .await
        .unwrap();
    assert!(epochs(resp.metadata()).is_some());
    let resp = resp.into_inner();
    assert_eq!(resp.version, 1);
    assert_eq!(resp.value, b"v1:6b31");
    // Written back by the lazy engine.
    assert_eq!(
        a_srv
            .ctx()
            .derived
            .get_stored("test_echo", Some(1), b"k1")
            .await
            .unwrap()
            .as_deref(),
        Some(b"v1:6b31".as_slice())
    );

    // Server B over the same storage registers v2 only (a version bump): the unchanged client
    // gets v2 by default and still reads the old v1 value by asking for version 1.
    let b_srv = InProcessServer::start(ServerTestOpts {
        storage: Some(a_srv.storage.clone()),
        registry: Some(registry_with(&[2])),
        ..opts()
    })
    .await;
    let mut b = q(&b_srv, Some(&alice())).await;
    let latest = b
        .get_derived(derived_req("test_echo", None, b"k1"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (latest.version, latest.value.as_slice()),
        (2, b"v2:6b31".as_slice())
    );
    let old = b
        .get_derived(derived_req("test_echo", Some(1), b"k1"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (old.version, old.value.as_slice()),
        (1, b"v1:6b31".as_slice())
    );
    // A v1 value that was never derived is not found once v1 is no longer registered.
    expect_code(
        b.get_derived(derived_req("test_echo", Some(1), b"never"))
            .await,
        ErrorCode::NotFound,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn get_derived_contract() {
    let srv = InProcessServer::start(ServerTestOpts {
        registry: Some(registry_with(&[1, 2])),
        ..opts()
    })
    .await;
    let ctx = srv.ctx();
    let mut a = q(&srv, Some(&alice())).await;
    // Both registered versions are served side by side.
    for (v, want) in [
        (Some(1), "v1:00ff"),
        (Some(2), "v2:00ff"),
        (None, "v2:00ff"),
    ] {
        let r = a
            .get_derived(derived_req("test_echo", v, &[0x00, 0xff]))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(r.value, want.as_bytes());
        assert_eq!(r.version, v.unwrap_or(2));
    }
    // A v1 type through the same path: git_mapping of an imported changeset.
    let imported = import_commit(&ctx, [0x11; 20], &[("f", b"f\n")]).await;
    let key = format!("g:{}", hex::encode([0x11u8; 20]));
    let r = a
        .get_derived(derived_req("git_mapping", None, key.as_bytes()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.value, idb(&imported));
    assert_eq!(r.version, 1);

    // NOT_FOUND: unknown type, and a registered non-lazy type with no value for the key.
    expect_code(
        a.get_derived(derived_req("no_such_type", None, b"k")).await,
        ErrorCode::NotFound,
    );
    expect_code(
        a.get_derived(derived_req("no_such_type", Some(1), b"k"))
            .await,
        ErrorCode::NotFound,
    );
    expect_code(
        a.get_derived(derived_req("git_mapping", None, b"g:nothing"))
            .await,
        ErrorCode::NotFound,
    );
    let e = expect_code(
        a.get_derived(derived_req("", None, b"k")).await,
        ErrorCode::InvalidArgument,
    );
    assert_eq!(e.subcode(), Some(subcode::MISSING_FIELD));
    // Content-bearing or not, every type needs repository read.
    for who in [mallory(), dave()] {
        expect_code(
            q(&srv, Some(&who))
                .await
                .get_derived(derived_req("test_echo", None, b"k"))
                .await,
            ErrorCode::PermissionDenied,
        );
    }
    expect_code(
        q(&srv, None)
            .await
            .get_derived(derived_req("test_echo", None, b"k"))
            .await,
        ErrorCode::Unauthenticated,
    );
}
