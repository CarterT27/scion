//! Shared helpers of the P27 query service tests.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use scion_core::{ChangeId, ErrorCode, Mode, ObjectId, Principal, ScionError};
use scion_history::{AllowAll, CreateRequest, MetaFields, MetaSource};
use scion_object::{DeltaEntry, DeltaOp, Extensions, RepoPath, Timestamp};
use scion_proto::{Epochs, read_epochs};
use scion_server::AppContext;
use scion_server::testing::{InProcessServer, ServerTestOpts};
use std::sync::Arc;

/// Test ACL: group `eng` reads `/`; `/secret` is readable only by group `secret` (carol and
/// the admins); `/team` is additionally readable by `github:dave`, who cannot read `/`.
/// `github:mallory` is in no group.
pub const ACL: &str = r#"version = 1

[groups]
eng = ["github:alice", "github:bob", "github:carol", "service:importer", "group:admins"]
secret = ["github:carol", "group:admins"]
admins = ["github:admin"]

[[paths]]
prefix = "/"
read = ["group:eng"]
write = ["group:eng"]

[[paths]]
prefix = "/secret"
read = ["group:secret"]
write = ["group:secret"]

[[paths]]
prefix = "/team"
read = ["group:eng", "github:dave"]
write = ["group:eng"]

[refs."*"]
update = ["group:eng"]
force = ["group:admins"]

[admin]
redact = ["group:admins"]
purge = ["group:admins"]
refs = ["group:admins"]
"#;

pub fn gh(login: &str) -> Principal {
    Principal::GitHub {
        login: login.into(),
        id: None,
    }
}

pub fn alice() -> Principal {
    gh("alice")
}
pub fn carol() -> Principal {
    gh("carol")
}
pub fn dave() -> Principal {
    gh("dave")
}
pub fn mallory() -> Principal {
    gh("mallory")
}
pub fn admin() -> Principal {
    gh("admin")
}

pub fn opts() -> ServerTestOpts {
    ServerTestOpts {
        acl_toml: ACL.to_owned(),
        ..ServerTestOpts::default()
    }
}

pub fn path(s: &str) -> RepoPath {
    RepoPath::parse(s.as_bytes()).unwrap()
}

pub fn meta_fields(msg: &str) -> MetaFields {
    MetaFields {
        author: b"Test <t@example.com>".to_vec(),
        author_time: Timestamp {
            seconds: 1_700_000_000,
            tz_offset_minutes: 0,
        },
        committer: None,
        committer_time: None,
        message: msg.as_bytes().to_vec(),
        extensions: Extensions::new(),
    }
}

/// Upload `content` as a blob and return its id.
pub async fn blob(ctx: &AppContext, content: &[u8]) -> ObjectId {
    ctx.objects
        .put_blob(Bytes::copy_from_slice(content))
        .await
        .unwrap()
        .0
}

/// The delta putting `files` (regular files) and deleting `deletes`, sorted by path.
pub async fn delta(ctx: &AppContext, files: &[(&str, &[u8])], deletes: &[&str]) -> Vec<DeltaEntry> {
    let mut d = Vec::new();
    for (p, content) in files {
        let id = blob(ctx, content).await;
        d.push(DeltaEntry {
            path: path(p),
            op: DeltaOp::Put {
                mode: Mode::File,
                id,
                size: content.len() as u64,
            },
        });
    }
    for p in deletes {
        d.push(DeltaEntry {
            path: path(p),
            op: DeltaOp::Delete,
        });
    }
    d.sort_by(|a, b| a.path.cmp(&b.path));
    d
}

/// Create a changeset directly through History (CreateChangeset is another package's RPC).
pub async fn commit(
    ctx: &AppContext,
    parents: &[ObjectId],
    files: &[(&str, &[u8])],
    deletes: &[&str],
) -> ObjectId {
    let d = delta(ctx, files, deletes).await;
    ctx.history
        .create_internal(
            parents.to_vec(),
            d,
            MetaSource::New(meta_fields("test")),
            ChangeId::random(),
            &alice(),
        )
        .await
        .unwrap()
        .changeset_id
}

/// An import-mode changeset (parentless) whose Meta carries git commit `sha` (extension tag
/// 2), so the eager `git_mapping` type records it (PLAN 6.4).
pub async fn import_commit(ctx: &AppContext, sha: [u8; 20], files: &[(&str, &[u8])]) -> ObjectId {
    let d = delta(ctx, files, &[]).await;
    let mut meta = meta_fields("imported");
    meta.extensions
        .insert(scion_object::META_EXT_GIT_COMMIT, sha.to_vec())
        .unwrap();
    ctx.history
        .create_changeset(
            CreateRequest {
                parents: vec![],
                delta: d,
                meta,
                change_id: ChangeId::random(),
                expected_root_tree: None,
                import_mode: true,
            },
            &Principal::Service("importer".into()),
            &AllowAll,
        )
        .await
        .unwrap()
        .changeset_id
}

pub const README_1: &[u8] = b"hello\n";
pub const README_2: &[u8] = b"hello\nworld\n";
pub const BIN_1: &[u8] = b"\x00\x01binary";
pub const BIN_2: &[u8] = b"\x00\x02binary";

/// A small repository:
///
/// - `c1` (child of the root): `README`, `bin.dat`, `src/main.rs`, `secret/key.txt`,
///   `secret/sub/deep.txt`, `team/notes.txt`;
/// - `c2` (child of `c1`): modifies `README`, `bin.dat` and `secret/key.txt`, adds
///   `team/b.txt`;
/// - `c3` (child of `c2`): modifies only `README` (every delta path readable by alice).
pub struct Repo {
    pub srv: InProcessServer,
    pub ctx: Arc<AppContext>,
    pub root: ObjectId,
    pub c1: ObjectId,
    pub c2: ObjectId,
    pub c3: ObjectId,
}

pub async fn repo() -> Repo {
    repo_with(opts()).await
}

pub async fn repo_with(o: ServerTestOpts) -> Repo {
    let srv = InProcessServer::start(o).await;
    let ctx = srv.ctx();
    let root = ctx
        .history
        .resolve_ref(&scion_object::RefName::main(), None)
        .await
        .unwrap()
        .target;
    let c1 = commit(
        &ctx,
        &[root],
        &[
            ("README", README_1),
            ("bin.dat", BIN_1),
            ("src/main.rs", b"fn main() {}\n"),
            ("secret/key.txt", b"s3cr3t-1\n"),
            ("secret/sub/deep.txt", b"deep\n"),
            ("team/notes.txt", b"notes\n"),
        ],
        &[],
    )
    .await;
    let c2 = commit(
        &ctx,
        &[c1],
        &[
            ("README", README_2),
            ("bin.dat", BIN_2),
            ("secret/key.txt", b"s3cr3t-2\n"),
            ("team/b.txt", b"b\n"),
        ],
        &[],
    )
    .await;
    let c3 = commit(&ctx, &[c2], &[("README", b"hello\nworld\n!\n")], &[]).await;
    Repo {
        srv,
        ctx,
        root,
        c1,
        c2,
        c3,
    }
}

pub fn idb(id: &ObjectId) -> Vec<u8> {
    id.as_bytes().to_vec()
}

/// The epochs carried by a response's metadata.
pub fn epochs(md: &tonic::metadata::MetadataMap) -> Option<Epochs> {
    read_epochs(&md.clone().into_headers())
}

/// The decoded SPEC 7.2 error of a status, after checking its trailers.
pub fn err_of(s: &tonic::Status) -> ScionError {
    assert!(
        epochs(s.metadata()).is_some(),
        "error without trailers: {s:?}"
    );
    scion_proto::error_from_status(s)
}

/// Assert `r` failed with `code` (and the matching gRPC code), with trailers.
#[track_caller]
pub fn expect_code<T>(r: Result<T, tonic::Status>, code: ErrorCode) -> ScionError {
    let s = match r {
        Ok(_) => panic!("expected {}, got Ok", code.as_str()),
        Err(s) => s,
    };
    assert_eq!(
        s.code(),
        scion_proto::tonic_code(code.grpc_code()),
        "grpc code of {s:?}"
    );
    let e = err_of(&s);
    assert_eq!(e.code, code, "{s:?}");
    e
}
