//! Shared helpers of the P25 object, upload, access and admin tests.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use scion_core::{ChangeId, Mode, ObjectId, Principal};
use scion_history::{AllowAll, CreateRequest, MetaFields};
use scion_object::{DeltaEntry, DeltaOp, Extensions, RefName, RepoPath, Timestamp};
use scion_proto::{ScionClient, v1};
use scion_server::testing::{InProcessServer, ServerTestOpts};
use std::collections::BTreeMap;

/// ACL used by the P25 tests: `eng` reads/writes `/`; `/secrets` is readable only by
/// `security` (carol and `github:sec`, who cannot read `/`); `github:dave` is in no group;
/// `github:admin` holds every admin permission; `github:redactor` holds only `admin.redact`.
pub const ACL: &str = r#"version = 1

[groups]
eng = ["github:alice", "github:bob", "github:carol", "group:admins"]
security = ["github:carol", "github:sec"]
admins = ["github:admin"]

[[paths]]
prefix = "/"
read = ["group:eng"]
write = ["group:eng"]

[[paths]]
prefix = "/secrets"
read = ["group:security"]
write = ["group:security"]

[refs."*"]
update = ["group:eng"]
force = ["group:admins"]

[admin]
redact = ["group:admins", "github:redactor"]
purge = ["group:admins"]
refs = ["group:admins"]
"#;

/// [`ACL`] plus a new `/restricted` rule readable only by `security`.
pub const ACL_RESTRICTED: &str = r#"version = 1

[groups]
eng = ["github:alice", "github:bob", "github:carol", "group:admins"]
security = ["github:carol", "github:sec"]
admins = ["github:admin"]

[[paths]]
prefix = "/"
read = ["group:eng"]
write = ["group:eng"]

[[paths]]
prefix = "/secrets"
read = ["group:security"]
write = ["group:security"]

[[paths]]
prefix = "/restricted"
read = ["group:security"]
write = ["group:security"]

[refs."*"]
update = ["group:eng"]
force = ["group:admins"]

[admin]
redact = ["group:admins", "github:redactor"]
purge = ["group:admins"]
refs = ["group:admins"]
"#;

pub fn opts() -> ServerTestOpts {
    ServerTestOpts {
        acl_toml: ACL.to_owned(),
        ..ServerTestOpts::default()
    }
}

pub async fn start() -> InProcessServer {
    InProcessServer::start(opts()).await
}

pub fn gh(login: &str) -> Principal {
    Principal::GitHub {
        login: login.into(),
        id: None,
    }
}

pub fn alice() -> Principal {
    gh("alice")
}
pub fn bob() -> Principal {
    gh("bob")
}
pub fn carol() -> Principal {
    gh("carol")
}
/// Reads `/secrets` only.
pub fn sec() -> Principal {
    gh("sec")
}
/// Reads nothing.
pub fn dave() -> Principal {
    gh("dave")
}
pub fn admin() -> Principal {
    gh("admin")
}
/// Holds `admin.redact` only.
pub fn redactor() -> Principal {
    gh("redactor")
}

pub fn fields(message: &str) -> MetaFields {
    MetaFields {
        author: b"Alice <alice@example.com>".to_vec(),
        author_time: Timestamp {
            seconds: 1_700_000_000,
            tz_offset_minutes: 0,
        },
        committer: None,
        committer_time: None,
        message: message.as_bytes().to_vec(),
        extensions: Extensions::new(),
    }
}

/// The result of [`commit`].
pub struct Committed {
    pub changeset: ObjectId,
    pub root_tree: ObjectId,
    pub meta: ObjectId,
    /// Blob id per path.
    pub blobs: BTreeMap<String, ObjectId>,
}

/// Store the blobs and create a changeset on top of `main` (directly through History; the
/// CreateChangeset RPC belongs to another package). The ref is not moved.
pub async fn commit(srv: &InProcessServer, files: &[(&str, &[u8])]) -> Committed {
    let ctx = srv.ctx();
    let main = ctx.history.resolve_ref(&RefName::main(), None).await.unwrap();
    let mut blobs = BTreeMap::new();
    let mut delta = Vec::new();
    for (path, content) in files {
        let (id, _) = ctx
            .objects
            .put_blob(Bytes::copy_from_slice(content))
            .await
            .unwrap();
        blobs.insert((*path).to_owned(), id);
        delta.push(DeltaEntry {
            path: RepoPath::parse(path.as_bytes()).unwrap(),
            op: DeltaOp::Put {
                mode: Mode::File,
                id,
                size: content.len() as u64,
            },
        });
    }
    delta.sort_by(|a, b| a.path.cmp(&b.path));
    let created = ctx
        .history
        .create_changeset(
            CreateRequest {
                parents: vec![main.target],
                delta,
                meta: fields("test"),
                change_id: ChangeId::random(),
                expected_root_tree: None,
                import_mode: false,
            },
            &alice(),
            &AllowAll,
        )
        .await
        .unwrap();
    Committed {
        changeset: created.changeset_id,
        root_tree: created.root_tree,
        meta: created.meta_id,
        blobs,
    }
}

/// One GetObjects item, reassembled from its chunks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: ObjectId,
    pub status: v1::ItemStatus,
    pub encoding: v1::Encoding,
    /// Data as sent (a zstd frame for ZSTD).
    pub data: Vec<u8>,
    pub total_size: u64,
    pub access_class: v1::AccessClass,
    pub first_offset: u64,
    pub chunks: usize,
}

impl Item {
    /// The decompressed content.
    pub fn content(&self) -> Vec<u8> {
        match self.encoding {
            v1::Encoding::Zstd => zstd::stream::decode_all(&self.data[..]).unwrap(),
            _ => self.data.clone(),
        }
    }
}

pub fn ids(v: &[ObjectId]) -> Vec<Vec<u8>> {
    v.iter().map(|i| i.as_bytes().to_vec()).collect()
}

/// GetObjects: the items in arrival order plus the error that ended the stream, if any.
pub async fn get_objects(
    c: &ScionClient,
    want: &[ObjectId],
    offset: Option<u64>,
    length: Option<u64>,
) -> Result<(Vec<Item>, Option<tonic::Status>), tonic::Status> {
    let mut s = c
        .objects()
        .get_objects(v1::GetObjectsRequest {
            ids: ids(want),
            offset,
            length,
        })
        .await?
        .into_inner();
    let mut items: Vec<Item> = Vec::new();
    let mut open = false;
    loop {
        match s.message().await {
            Ok(Some(ch)) => {
                let id = ObjectId::from_bytes(&ch.id).unwrap();
                if open {
                    let last = items.last_mut().unwrap();
                    assert_eq!(last.id, id, "chunks of one item are contiguous");
                    assert_eq!(ch.offset, last.first_offset + last.data.len() as u64);
                    last.data.extend_from_slice(&ch.data);
                    last.chunks += 1;
                } else {
                    items.push(Item {
                        id,
                        status: v1::ItemStatus::try_from(ch.status).unwrap(),
                        encoding: v1::Encoding::try_from(ch.encoding).unwrap(),
                        data: ch.data.clone(),
                        total_size: ch.total_size,
                        access_class: v1::AccessClass::try_from(ch.access_class).unwrap(),
                        first_offset: ch.offset,
                        chunks: 1,
                    });
                }
                open = !ch.r#final;
            }
            Ok(None) => return Ok((items, None)),
            Err(e) => return Ok((items, Some(e))),
        }
    }
}

/// GetObjects of one id that must be OK: its decompressed content.
pub async fn read_ok(c: &ScionClient, id: &ObjectId) -> Vec<u8> {
    let (items, err) = get_objects(c, &[*id], None, None).await.unwrap();
    assert!(err.is_none(), "{err:?}");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].status, v1::ItemStatus::Ok, "{id}");
    items[0].content()
}

/// The status of one id through GetObjects.
pub async fn status_of(c: &ScionClient, id: &ObjectId) -> v1::ItemStatus {
    let (items, err) = get_objects(c, &[*id], None, None).await.unwrap();
    assert!(err.is_none(), "{err:?}");
    assert_eq!(items.len(), 1);
    items[0].status
}

pub async fn has(c: &ScionClient, want: &[ObjectId]) -> Vec<bool> {
    c.objects()
        .has_objects(v1::HasObjectsRequest { ids: ids(want) })
        .await
        .unwrap()
        .into_inner()
        .present
}

/// UploadObjects of `(id, kind, data)` items.
pub async fn upload(
    c: &ScionClient,
    items: Vec<(Vec<u8>, u32, Vec<u8>)>,
) -> Result<Vec<v1::UploadResult>, tonic::Status> {
    let msgs: Vec<v1::UploadItem> = items
        .into_iter()
        .map(|(id, kind, data)| v1::UploadItem { id, kind, data })
        .collect();
    Ok(c.write()
        .upload_objects(tokio_stream::iter(msgs))
        .await?
        .into_inner()
        .results)
}

/// Upload one blob; returns its id and result.
pub async fn upload_blob(c: &ScionClient, content: &[u8]) -> (ObjectId, v1::UploadResult) {
    let id = ObjectId::hash_blob(content);
    let mut r = upload(c, vec![(id.as_bytes().to_vec(), 1, content.to_vec())])
        .await
        .unwrap();
    (id, r.remove(0))
}

/// The SPEC 7.2 code carried by a status.
pub fn code(s: &tonic::Status) -> scion_core::ErrorCode {
    scion_proto::error_from_status(s).code
}

pub fn subcode(s: &tonic::Status) -> Option<String> {
    scion_proto::error_from_status(s)
        .subcode()
        .map(|x| x.to_owned())
}

/// The epochs carried by a response's metadata.
pub fn epochs(md: &tonic::metadata::MetadataMap) -> Option<scion_proto::Epochs> {
    scion_proto::read_epochs(&md.clone().into_headers())
}
