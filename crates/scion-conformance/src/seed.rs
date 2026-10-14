//! Seeding generated histories through RPCs (PLAN 2.22; SPEC 19.0 corpus, 7.5; P30).
//!
//! Every commit of a [`GenRepo`] becomes one `CreateChangeset` call: the blobs its delta needs
//! are uploaded first with `UploadObjects` (each item must come back `ok`), the delta is the
//! file-level diff from the first parent's snapshot (SPEC 4.3.4), and `expected_root_tree` is
//! the production encoding of the generated snapshot, so the server must build exactly the
//! generated tree. Parentless commits use `import_mode` (PLAN 6.4), which the seeding
//! principal is allowed to use.

use bytes::Bytes;
use scion_core::{ErrorCode, Kind, ObjectId, Result, ScionError};
use scion_proto::convert::{DELTA_OP_DELETE, DELTA_OP_PUT};
use scion_proto::{ScionClient, error_from_status, v1};
use scion_refmodel::{MDelta, MTree, file_diff};
use scion_testkit::{GenRepo, encode_tree};
use std::collections::BTreeSet;

/// Author time of seeded commit `i` (deterministic; SPEC 4.3.3).
fn author_time(i: usize) -> v1::Timestamp {
    v1::Timestamp {
        seconds: 1_700_000_000 + i as i64,
        tz_offset_minutes: 0,
    }
}

fn bad(msg: impl Into<String>) -> ScionError {
    ScionError::new(ErrorCode::InvalidArgument, msg)
}

/// Upload `blobs` (raw contents) in one `UploadObjects` stream; every item must be `ok`
/// (SPEC 7.5). Returns their ids in order.
pub(crate) async fn upload_blobs(client: &ScionClient, blobs: &[Bytes]) -> Result<Vec<ObjectId>> {
    let ids: Vec<ObjectId> = blobs.iter().map(|b| ObjectId::hash_blob(b)).collect();
    if blobs.is_empty() {
        return Ok(ids);
    }
    let items: Vec<v1::UploadItem> = blobs
        .iter()
        .zip(&ids)
        .map(|(b, id)| v1::UploadItem {
            id: id.as_bytes().to_vec(),
            kind: u32::from(Kind::Blob.byte()),
            data: b.to_vec(),
        })
        .collect();
    let resp = client
        .write()
        .upload_objects(futures::stream::iter(items))
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    if resp.results.len() != ids.len() {
        return Err(bad(format!(
            "UploadObjects returned {} results for {} items",
            resp.results.len(),
            ids.len()
        )));
    }
    for (r, id) in resp.results.iter().zip(&ids) {
        if r.status != v1::ItemStatus::Ok as i32 {
            return Err(bad(format!(
                "upload of {id} failed: {} {}",
                r.error_code, r.message
            )));
        }
    }
    Ok(ids)
}

/// The proto delta from `base` to `tree` (SPEC 4.3.4), sorted by path, plus the blob
/// contents it puts.
fn delta_of(base: &MTree, tree: &MTree) -> (Vec<v1::DeltaEntryMsg>, Vec<Bytes>) {
    let mut delta = Vec::new();
    let mut blobs = Vec::new();
    for d in file_diff(base, tree) {
        match d {
            MDelta::Put(path, f) => {
                let id = ObjectId::hash_blob(&f.content);
                delta.push(v1::DeltaEntryMsg {
                    path,
                    op: DELTA_OP_PUT,
                    mode: u32::from(f.mode),
                    id: id.as_bytes().to_vec(),
                    size: f.content.len() as u64,
                });
                blobs.push(Bytes::from(f.content));
            }
            MDelta::Delete(path) => delta.push(v1::DeltaEntryMsg {
                path,
                op: DELTA_OP_DELETE,
                mode: 0,
                id: Vec::new(),
                size: 0,
            }),
        }
    }
    delta.sort_by(|a, b| a.path.cmp(&b.path));
    (delta, blobs)
}

/// Seed `repo` through `client` (PLAN 2.22): returns the changeset id of every commit, by
/// index.
pub(crate) async fn seed_repo(client: &ScionClient, repo: &GenRepo) -> Result<Vec<ObjectId>> {
    let empty = MTree::new();
    let mut ids: Vec<ObjectId> = Vec::with_capacity(repo.commits.len());
    let mut uploaded: BTreeSet<ObjectId> = BTreeSet::new();
    for (i, c) in repo.commits.iter().enumerate() {
        let mut parents = Vec::with_capacity(c.parents.len());
        for &p in &c.parents {
            let id = ids
                .get(p)
                .ok_or_else(|| bad(format!("commit {i} names a later parent {p}")))?;
            parents.push(id.as_bytes().to_vec());
        }
        let base = match c.parents.first() {
            Some(&p) => &repo.commits[p].tree,
            None => &empty,
        };
        let (delta, blobs) = delta_of(base, &c.tree);
        let fresh: Vec<Bytes> = blobs
            .into_iter()
            .filter(|b| uploaded.insert(ObjectId::hash_blob(b)))
            .collect();
        upload_blobs(client, &fresh).await?;
        let (root, _) = encode_tree(&c.tree);
        let req = v1::CreateChangesetRequest {
            parents,
            delta,
            meta_fields: Some(v1::MetaFields {
                author: c.author.clone(),
                author_time: Some(author_time(i)),
                committer: None,
                committer_time: None,
                message: c.message.clone(),
                extensions: Vec::new(),
            }),
            change_id: c.change_id.to_vec(),
            expected_root_tree: Some(root.as_bytes().to_vec()),
            import_mode: c.parents.is_empty(),
        };
        let resp = client
            .write()
            .create_changeset(req)
            .await
            .map_err(|s| error_from_status(&s))?
            .into_inner();
        let id = ObjectId::from_bytes(&resp.changeset_id)?;
        ids.push(id);
    }
    Ok(ids)
}
