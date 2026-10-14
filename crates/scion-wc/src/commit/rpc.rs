//! RPC helpers of the write layer (SPEC 7.3-7.5, 8.2, 8.4; P28).

use super::journal::CommitRequest;
use scion_core::{ChangeId, ErrorCode, Kind, ObjectId, Result, ScionError, subcode};
use scion_object::{Canonical, Changeset, DeltaEntry, Meta, RefName};
use scion_proto::convert::{
    CreateChangesetParts, MetaFieldsParts, delta_from_proto, id_bytes, ids_bytes,
    required_change_id, required_id,
};
use scion_proto::{ScionClient, error_from_status, v1};

/// Ids per HasObjects request.
const HAS_BATCH: usize = 1024;
/// Upload payload bytes per UploadObjects stream.
const UPLOAD_BATCH_BYTES: usize = 32 << 20;

/// A changeset as seen by the caller (SPEC 7.4 GetChangesets; a filtered view when some delta
/// path is unreadable, SPEC 12.6).
#[derive(Clone, Debug)]
pub(crate) struct CsInfo {
    pub parents: Vec<ObjectId>,
    pub root_tree: ObjectId,
    pub meta: ObjectId,
    pub change_id: ChangeId,
    pub delta: Vec<DeltaEntry>,
    pub filtered: bool,
}

fn upstream(msg: &str) -> ScionError {
    ScionError::unavailable(subcode::UPSTREAM_UNAVAILABLE, msg)
}

/// GetChangesets for one id; a full encoding is verified against `id`.
pub(crate) async fn fetch_changeset(client: &ScionClient, id: &ObjectId) -> Result<CsInfo> {
    let req = v1::GetChangesetsRequest {
        ids: vec![id_bytes(id)],
    };
    let resp = client
        .query()
        .get_changesets(req)
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    let want = id_bytes(id);
    let item = resp
        .items
        .into_iter()
        .find(|i| i.id == want)
        .ok_or_else(|| upstream("GetChangesets response omitted the requested changeset"))?;
    match v1::ItemStatus::try_from(item.status) {
        Ok(v1::ItemStatus::Ok) => {}
        Ok(v1::ItemStatus::NotFound) => {
            return Err(ScionError::not_found(format!("changeset {id} not found")));
        }
        Ok(v1::ItemStatus::Denied) => {
            return Err(ScionError::permission_denied(format!(
                "changeset {id} is not readable"
            )));
        }
        Ok(v1::ItemStatus::Redacted) => return Err(ScionError::redacted(id)),
        _ => {
            return Err(upstream(
                "GetChangesets returned an unspecified item status",
            ));
        }
    }
    match item.view {
        Some(v1::changeset_item::View::Encoded(enc)) => {
            if ObjectId::hash_encoded(Kind::Changeset, &enc) != *id {
                return Err(ScionError::integrity(id));
            }
            let cs = Changeset::decode(&enc)?;
            Ok(CsInfo {
                parents: cs.parents,
                root_tree: cs.root_tree,
                meta: cs.meta,
                change_id: cs.change_id,
                delta: cs.delta,
                filtered: false,
            })
        }
        Some(v1::changeset_item::View::FilteredView(f)) => Ok(CsInfo {
            parents: f
                .parents
                .iter()
                .map(|p| required_id(p, "parents"))
                .collect::<Result<_>>()?,
            root_tree: required_id(&f.root_tree, "root_tree")?,
            meta: required_id(&f.meta, "meta")?,
            change_id: required_change_id(&f.change_id, "change_id")?,
            delta: delta_from_proto(&f.delta)?,
            filtered: f.filtered,
        }),
        None => Err(scion_proto::convert::missing_field("view")),
    }
}

/// The client-supplied Meta fields of an existing Meta (everything but `creator`).
pub(crate) fn meta_fields_of(m: Meta) -> MetaFieldsParts {
    MetaFieldsParts {
        author: m.author,
        author_time: m.author_time,
        committer: m.committer,
        committer_time: m.committer_time,
        message: m.message,
        extensions: m.extensions,
    }
}

/// HasObjects (SPEC 7.3, 12.5): `true` when the object exists and the caller may read it or
/// uploaded it.
pub(crate) async fn has_objects(client: &ScionClient, ids: &[ObjectId]) -> Result<Vec<bool>> {
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(HAS_BATCH) {
        let resp = client
            .objects()
            .has_objects(v1::HasObjectsRequest {
                ids: ids_bytes(chunk),
            })
            .await
            .map_err(|s| error_from_status(&s))?
            .into_inner();
        if resp.present.len() != chunk.len() {
            return Err(upstream("HasObjects answered a different number of ids"));
        }
        out.extend(resp.present);
    }
    Ok(out)
}

/// UploadObjects of blobs (SPEC 7.5), in streams of bounded payload size. Every item must be
/// accepted; a refused item fails the upload with its SPEC 7.2 code. Returns the item count.
pub(crate) async fn upload_blobs(
    client: &ScionClient,
    blobs: Vec<(ObjectId, Vec<u8>)>,
) -> Result<usize> {
    let total = blobs.len();
    let mut batch: Vec<v1::UploadItem> = Vec::new();
    let mut bytes = 0usize;
    for (id, data) in blobs {
        bytes += data.len();
        batch.push(v1::UploadItem {
            id: id_bytes(&id),
            kind: u32::from(Kind::Blob.byte()),
            data,
        });
        if bytes >= UPLOAD_BATCH_BYTES {
            send_upload(client, std::mem::take(&mut batch)).await?;
            bytes = 0;
        }
    }
    if !batch.is_empty() {
        send_upload(client, batch).await?;
    }
    Ok(total)
}

async fn send_upload(client: &ScionClient, items: Vec<v1::UploadItem>) -> Result<()> {
    let n = items.len();
    let resp = client
        .write()
        .upload_objects(tokio_stream::iter(items))
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    if resp.results.len() != n {
        return Err(upstream(
            "UploadObjects answered a different number of items",
        ));
    }
    for r in resp.results {
        if v1::ItemStatus::try_from(r.status) == Ok(v1::ItemStatus::Ok) {
            continue;
        }
        let id = ObjectId::from_bytes(&r.id)?;
        if v1::ItemStatus::try_from(r.status) == Ok(v1::ItemStatus::Redacted) {
            return Err(ScionError::redacted(&id));
        }
        let code = ErrorCode::parse(&r.error_code).unwrap_or(ErrorCode::Unavailable);
        return Err(ScionError::new(
            code,
            format!("upload of {id} refused: {}", r.message),
        ));
    }
    Ok(())
}

/// The ids returned by CreateChangeset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Created {
    pub changeset: ObjectId,
    pub root_tree: ObjectId,
}

/// CreateChangeset (SPEC 7.5, 8.1). Identical requests return identical ids.
pub(crate) async fn create_changeset(client: &ScionClient, req: &CommitRequest) -> Result<Created> {
    let parts = CreateChangesetParts {
        parents: req.parents.clone(),
        delta: req.delta.clone(),
        meta: req.meta.clone(),
        change_id: req.change_id,
        expected_root_tree: None,
        import_mode: false,
    };
    let resp = client
        .write()
        .create_changeset(v1::CreateChangesetRequest::from(&parts))
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    let changeset = required_id(&resp.changeset_id, "changeset_id")?;
    let root_tree = required_id(&resp.root_tree, "root_tree")?;
    if changeset.kind() != Kind::Changeset || root_tree.kind() != Kind::Tree {
        return Err(upstream("CreateChangeset returned ids of the wrong kind"));
    }
    Ok(Created {
        changeset,
        root_tree,
    })
}

/// Server Rebase of one changeset (SPEC 8.2): MERGE_CONFLICT (with its report) and
/// EMPTY_CHANGE pass through.
pub(crate) async fn rebase(
    client: &ScionClient,
    c: &ObjectId,
    onto: &ObjectId,
) -> Result<ObjectId> {
    let resp = client
        .write()
        .rebase(v1::RebaseRequest {
            changeset: id_bytes(c),
            onto: id_bytes(onto),
        })
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    required_id(&resp.changeset_id, "changeset_id")
}

/// IsAncestor (SPEC 7.4): `a` is `b` or an ancestor of it.
pub(crate) async fn is_ancestor(client: &ScionClient, a: &ObjectId, b: &ObjectId) -> Result<bool> {
    let resp = client
        .query()
        .is_ancestor(v1::IsAncestorRequest {
            a: id_bytes(a),
            b: id_bytes(b),
        })
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    Ok(resp.result)
}

/// The outcome of a Land (SPEC 8.4).
#[derive(Clone, Debug)]
pub(crate) struct Landed {
    pub target: ObjectId,
    pub version: u64,
    /// `(source, result)` per landed member, oldest first.
    pub landed: Vec<(ObjectId, ObjectId)>,
}

/// Land (SPEC 7.5, 8.4) without `allow_empty`.
pub(crate) async fn land(client: &ScionClient, r: &RefName, head: &ObjectId) -> Result<Landed> {
    let resp = client
        .write()
        .land(v1::LandRequest {
            r#ref: r.as_str().to_owned(),
            head: id_bytes(head),
            allow_empty: false,
        })
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    let mut landed = Vec::with_capacity(resp.landed.len());
    for l in &resp.landed {
        landed.push((
            required_id(&l.source, "source")?,
            required_id(&l.result, "result")?,
        ));
    }
    Ok(Landed {
        target: required_id(&resp.target, "target")?,
        version: resp.version,
        landed,
    })
}
