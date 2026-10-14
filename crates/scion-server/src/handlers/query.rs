//! Handlers: GetChangesets, ReadPath, ListTree, Diff, IsAncestor, MergeBase, Resolve (SPEC 7.4) (P27).
//!
//! Authorization (SPEC 12.4, 12.6; implementation-defined 6.12): every query requires
//! repository read (read on `/`), except `ReadPath` and `ListTree`, which require read on the
//! requested path instead. History filtering (SPEC 12.6):
//!
//! - `GetChangesets` returns the canonical encoding only when every delta path is readable, and
//!   otherwise a `filtered_view` without the unreadable delta entries (`filtered = true`);
//! - `Diff` requires read on every requested scope path and omits unreadable entries;
//! - `ListTree` shows restricted subdirectory names and ids in a readable directory (the same
//!   information the tree object itself carries; SPEC 12.6 limitation, Appendix A).
//!
//! Policy decisions are taken against one policy snapshot per request. Denials never name the
//! denied path (PLAN 6.9).

use crate::app::AppContext;
use crate::context::caller;
use crate::errors::{StatusExt, to_status};
use crate::handlers::BoxStream;
use crate::logging::annotate;
use bytes::Bytes;
use futures::StreamExt;
use scion_core::{EntryInfo, ErrorCode, Kind, Mode, ObjectId, Principal, ScionError, subcode};
use scion_history::FileChange;
use scion_object::{Changeset, RepoPath};
use scion_policy::Policy;
use scion_proto::convert::{
    delta_to_proto, id_bytes, ids_bytes, missing_field, opt_entry_to_proto, optional_id, parse_ids,
    required_id,
};
use scion_proto::v1;
use scion_store::ChangesetRow;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// Content bytes per `ReadPathChunk` (SPEC 7.4 content stream).
pub const READ_PATH_CHUNK_BYTES: usize = 1 << 20;

/// Context lines around each `Diff` text hunk (SPEC 7.4 `include_content_hunks`).
pub const DIFF_CONTEXT_LINES: usize = 3;

/// PERMISSION_DENIED without naming the path (PLAN 6.9).
fn denied(what: &str) -> Status {
    to_status(&ScionError::permission_denied(format!(
        "read access denied: {what}"
    )))
}

/// SPEC 12.4 repository read: read on `/`. Returns the policy snapshot used for the request.
fn require_repo_read(ctx: &AppContext, p: &Principal) -> Result<Arc<Policy>, Status> {
    let policy = ctx.policy.current();
    if policy.can_read(p, &RepoPath::root()) {
        Ok(policy)
    } else {
        Err(denied("repository read (read on /) required"))
    }
}

/// The root tree of changeset `id` (NOT_FOUND when absent; INVALID_ARGUMENT/invalid_kind for a
/// non-changeset id).
async fn root_tree(ctx: &AppContext, id: &ObjectId) -> Result<ObjectId, Status> {
    if id.kind() != Kind::Changeset {
        return Err(to_status(&ScionError::invalid(
            subcode::INVALID_KIND,
            format!("{id} is not a changeset id"),
        )));
    }
    Ok(ctx
        .history
        .changeset(id)
        .await
        .map_status()?
        .changeset
        .root_tree)
}

fn parse_path(b: &[u8]) -> Result<RepoPath, Status> {
    RepoPath::parse(b).map_status()
}

// ---------------------------------------------------------------------------------------------
// GetChangesets
// ---------------------------------------------------------------------------------------------

fn not_found_item(id: &ObjectId) -> v1::ChangesetItem {
    v1::ChangesetItem {
        id: id_bytes(id),
        status: v1::ItemStatus::NotFound as i32,
        view: None,
        generation: 0,
    }
}

/// SPEC 12.6: the full encoding when every delta path is readable, else the filtered view.
fn changeset_view(
    policy: &Policy,
    p: &Principal,
    cs: &Changeset,
    row: &ChangesetRow,
) -> v1::changeset_item::View {
    let all_readable = cs.delta.iter().all(|d| policy.can_read(p, &d.path));
    if all_readable {
        return v1::changeset_item::View::Encoded(row.encoded.to_vec());
    }
    let readable: Vec<_> = cs
        .delta
        .iter()
        .filter(|d| policy.can_read(p, &d.path))
        .cloned()
        .collect();
    v1::changeset_item::View::FilteredView(v1::FilteredChangeset {
        parents: ids_bytes(&cs.parents),
        root_tree: id_bytes(&cs.root_tree),
        meta: id_bytes(&cs.meta),
        change_id: cs.change_id.0.to_vec(),
        delta: delta_to_proto(&readable),
        filtered: true,
    })
}

/// `QueryService.GetChangesets` (SPEC 7.4, 12.6).
///
/// Requires repository read. Per item: `OK` with the canonical encoding (or the SPEC 12.6
/// filtered view when some delta path is unreadable) and the generation, or `NOT_FOUND` for an
/// absent changeset or a non-changeset id. A missing item never fails the batch (SPEC 7.1);
/// a malformed id is INVALID_ARGUMENT/bad_id_text for the whole request.
pub async fn get_changesets(
    ctx: &Arc<AppContext>,
    req: Request<v1::GetChangesetsRequest>,
) -> Result<Response<v1::GetChangesetsResponse>, Status> {
    let c = caller(&req)?;
    let policy = require_repo_read(ctx, &c.principal)?;
    let ids = parse_ids(&req.get_ref().ids, "ids").map_status()?;
    let mut items = Vec::with_capacity(ids.len());
    for id in &ids {
        if id.kind() != Kind::Changeset {
            items.push(not_found_item(id));
            continue;
        }
        match ctx.objects.get_changeset(id).await {
            Ok((cs, row)) => items.push(v1::ChangesetItem {
                id: id_bytes(id),
                status: v1::ItemStatus::Ok as i32,
                view: Some(changeset_view(&policy, &c.principal, &cs, &row)),
                generation: row.generation,
            }),
            Err(e) if e.code == ErrorCode::NotFound => items.push(not_found_item(id)),
            Err(e) => return Err(to_status(&e)),
        }
    }
    Ok(Response::new(v1::GetChangesetsResponse { items }))
}

// ---------------------------------------------------------------------------------------------
// ReadPath / ListTree
// ---------------------------------------------------------------------------------------------

/// The entry at `path` in changeset `cs`: the root tree for the root path (NOT_FOUND when the
/// path does not exist).
async fn entry_at(ctx: &AppContext, cs: &ObjectId, path: &RepoPath) -> Result<EntryInfo, Status> {
    let root = root_tree(ctx, cs).await?;
    if path.is_root() {
        return Ok(EntryInfo {
            mode: Mode::Tree,
            id: root,
            size: 0,
        });
    }
    match ctx.history.lookup(&root, path).await.map_status()? {
        Some(e) => Ok(EntryInfo {
            mode: e.mode,
            id: e.id,
            size: e.size,
        }),
        None => Err(to_status(&ScionError::not_found(format!(
            "path not found in {cs}"
        )))),
    }
}

/// Blob content, REDACTED when the object is redacted (SPEC 13.4; the in-memory redaction set
/// and the object row state are both consulted).
async fn blob_content(ctx: &AppContext, id: &ObjectId) -> scion_core::Result<Bytes> {
    if ctx.redactions.contains(id) {
        return Err(ScionError::redacted(id));
    }
    ctx.objects.get(id).await
}

fn read_path_chunks(entry: &EntryInfo, data: Bytes) -> Vec<v1::ReadPathChunk> {
    let mut chunks: Vec<v1::ReadPathChunk> = Vec::new();
    let mut rest = data;
    loop {
        let take = rest.len().min(READ_PATH_CHUNK_BYTES);
        let piece = rest.split_to(take);
        chunks.push(v1::ReadPathChunk {
            entry: chunks.is_empty().then(|| v1::EntryInfo::from(entry)),
            data: piece.to_vec(),
            r#final: rest.is_empty(),
        });
        if rest.is_empty() {
            return chunks;
        }
    }
}

/// `QueryService.ReadPath` (SPEC 7.4, 12.6, 13.4).
///
/// Requires read on `path` (PERMISSION_DENIED otherwise, checked before existence). The first
/// chunk carries the entry; file and symlink content follows in chunks of
/// [`READ_PATH_CHUNK_BYTES`], the last one with `final` set. A directory (including the root,
/// the empty path) returns only its entry. NOT_FOUND for a missing changeset or path; REDACTED
/// for a redacted blob (before any chunk is sent).
pub async fn read_path(
    ctx: &Arc<AppContext>,
    req: Request<v1::ReadPathRequest>,
) -> Result<Response<BoxStream<v1::ReadPathChunk>>, Status> {
    let c = caller(&req)?;
    let r = req.get_ref();
    let cs = required_id(&r.changeset, "changeset").map_status()?;
    let path = parse_path(&r.path)?;
    annotate(&req, |f| f.changeset_id = Some(cs.to_text()));
    if !ctx.policy.current().can_read(&c.principal, &path) {
        return Err(denied("read on the path required"));
    }
    let entry = entry_at(ctx, &cs, &path).await?;
    let data = if entry.mode.is_file_like() {
        blob_content(ctx, &entry.id).await.map_status()?
    } else {
        Bytes::new()
    };
    let chunks = read_path_chunks(&entry, data);
    let stream: BoxStream<v1::ReadPathChunk> = Box::pin(futures::stream::iter(
        chunks.into_iter().map(Ok::<_, Status>),
    ));
    Ok(Response::new(stream))
}

/// `QueryService.ListTree` (SPEC 7.4, 12.6).
///
/// Requires read on the directory (PERMISSION_DENIED otherwise). Returns every entry of the
/// tree, restricted subdirectories included (name and id only; fetching them is denied, SPEC
/// 12.6 limitation), plus the tree id. The empty path lists the root tree. NOT_FOUND for a
/// missing changeset or path; INVALID_ARGUMENT/invalid_path when the path is not a directory.
pub async fn list_tree(
    ctx: &Arc<AppContext>,
    req: Request<v1::ListTreeRequest>,
) -> Result<Response<v1::ListTreeResponse>, Status> {
    let c = caller(&req)?;
    let r = req.get_ref();
    let cs = required_id(&r.changeset, "changeset").map_status()?;
    let path = parse_path(&r.path)?;
    annotate(&req, |f| f.changeset_id = Some(cs.to_text()));
    if !ctx.policy.current().can_read(&c.principal, &path) {
        return Err(denied("read on the directory required"));
    }
    let entry = entry_at(ctx, &cs, &path).await?;
    if entry.mode != Mode::Tree {
        return Err(to_status(&ScionError::invalid(
            subcode::INVALID_PATH,
            "path is not a directory",
        )));
    }
    let tree = ctx.history.tree(&entry.id).await.map_status()?;
    Ok(Response::new(v1::ListTreeResponse {
        entries: tree.entries.iter().map(v1::TreeEntryMsg::from).collect(),
        tree_id: id_bytes(&entry.id),
    }))
}

// ---------------------------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------------------------

/// Content of one side of a file change for hunks: `Ok(None)` when it is unavailable because
/// it is redacted (no hunks are produced then).
async fn side_content(
    ctx: &AppContext,
    e: Option<&EntryInfo>,
) -> scion_core::Result<Option<Bytes>> {
    let Some(e) = e else {
        return Ok(Some(Bytes::new()));
    };
    match blob_content(ctx, &e.id).await {
        Ok(b) => Ok(Some(b)),
        Err(err) if err.code == ErrorCode::Redacted => Ok(None),
        Err(err) => Err(err),
    }
}

fn hunk_to_proto(h: scion_merge::TextHunk) -> v1::TextHunkMsg {
    v1::TextHunkMsg {
        old_start: h.old_start,
        old_lines: h.old_lines,
        new_start: h.new_start,
        new_lines: h.new_lines,
        lines: h
            .lines
            .into_iter()
            .map(|(tag, line)| {
                let mut v = Vec::with_capacity(line.len() + 1);
                v.push(tag);
                v.extend_from_slice(&line);
                v
            })
            .collect(),
    }
}

/// One `DiffItem`; with `hunks`, text changes carry unified hunks (SPEC 7.4) and binary changes
/// (a NUL in the first 8000 bytes of either side, or a side above `merge_max_text_bytes`) set
/// `binary`. A redacted side yields no hunks.
async fn diff_item(
    ctx: &AppContext,
    fc: FileChange,
    hunks: bool,
) -> scion_core::Result<v1::DiffItem> {
    let mut item = v1::DiffItem {
        path: fc.path.as_bytes().to_vec(),
        old_entry: opt_entry_to_proto(fc.old.as_ref()),
        new_entry: opt_entry_to_proto(fc.new.as_ref()),
        hunks: Vec::new(),
        binary: false,
    };
    if !hunks || fc.old.map(|e| e.id) == fc.new.map(|e| e.id) {
        return Ok(item);
    }
    let max = ctx.config.limits.merge_max_text_bytes;
    if [fc.old, fc.new].iter().flatten().any(|e| e.size > max) {
        item.binary = true;
        return Ok(item);
    }
    let (Some(a), Some(b)) = (
        side_content(ctx, fc.old.as_ref()).await?,
        side_content(ctx, fc.new.as_ref()).await?,
    ) else {
        return Ok(item);
    };
    if scion_merge::is_binary(&a) || scion_merge::is_binary(&b) {
        item.binary = true;
        return Ok(item);
    }
    item.hunks = scion_merge::unified_hunks(&a, &b, DIFF_CONTEXT_LINES)
        .into_iter()
        .map(hunk_to_proto)
        .collect();
    Ok(item)
}

/// `QueryService.Diff` (SPEC 7.4, 12.6, 4.3.4).
///
/// Requires repository read and read on every requested scope path (PERMISSION_DENIED
/// otherwise). FileDiff from changeset `a` (empty = the empty tree) to changeset `b`, limited to
/// `paths` when given; entries the caller cannot read are omitted. Items stream in ascending
/// path order. NOT_FOUND for a missing changeset; INVALID_ARGUMENT for a missing `b`, a
/// malformed id or path, or a non-changeset id.
pub async fn diff(
    ctx: &Arc<AppContext>,
    req: Request<v1::DiffRequest>,
) -> Result<Response<BoxStream<v1::DiffItem>>, Status> {
    let c = caller(&req)?;
    let policy = require_repo_read(ctx, &c.principal)?;
    let r = req.get_ref();
    let a = optional_id(&r.a, "a").map_status()?;
    let b = required_id(&r.b, "b").map_status()?;
    let scope = r
        .paths
        .iter()
        .map(|p| parse_path(p))
        .collect::<Result<Vec<_>, _>>()?;
    if scope.iter().any(|s| !policy.can_read(&c.principal, s)) {
        return Err(denied("read on every diff path required"));
    }
    let include_hunks = r.include_content_hunks;
    let ta = match &a {
        Some(a) => Some(root_tree(ctx, a).await?),
        None => None,
    };
    let tb = root_tree(ctx, &b).await?;
    let changes = ctx
        .history
        .file_diff(
            ta.as_ref(),
            &tb,
            (!scope.is_empty()).then_some(scope.as_slice()),
        )
        .await
        .map_status()?;
    let visible: Vec<FileChange> = changes
        .into_iter()
        .filter(|fc| policy.can_read(&c.principal, &fc.path))
        .collect();
    let ctx = ctx.clone();
    let stream = futures::stream::iter(visible).then(move |fc| {
        let ctx = ctx.clone();
        async move { diff_item(&ctx, fc, include_hunks).await.map_status() }
    });
    let stream: BoxStream<v1::DiffItem> = Box::pin(stream);
    Ok(Response::new(stream))
}

// ---------------------------------------------------------------------------------------------
// Graph queries and Resolve
// ---------------------------------------------------------------------------------------------

/// `QueryService.IsAncestor` (SPEC 7.4, 18.5): is `a` an ancestor of (or equal to) `b`?
/// Requires repository read; NOT_FOUND for a missing changeset; INVALID_ARGUMENT for a missing
/// or malformed id.
pub async fn is_ancestor(
    ctx: &Arc<AppContext>,
    req: Request<v1::IsAncestorRequest>,
) -> Result<Response<v1::IsAncestorResponse>, Status> {
    let c = caller(&req)?;
    require_repo_read(ctx, &c.principal)?;
    let r = req.get_ref();
    let a = required_id(&r.a, "a").map_status()?;
    let b = required_id(&r.b, "b").map_status()?;
    let result = ctx.history.is_ancestor(&a, &b).await.map_status()?;
    Ok(Response::new(v1::IsAncestorResponse { result }))
}

/// `QueryService.MergeBase` (SPEC 7.4, 8.3): every best common ancestor, ascending by id (empty
/// for unrelated histories). Requires repository read; NOT_FOUND for a missing changeset;
/// INVALID_ARGUMENT for a missing or malformed id.
pub async fn merge_base(
    ctx: &Arc<AppContext>,
    req: Request<v1::MergeBaseRequest>,
) -> Result<Response<v1::MergeBaseResponse>, Status> {
    let c = caller(&req)?;
    require_repo_read(ctx, &c.principal)?;
    let r = req.get_ref();
    let a = required_id(&r.a, "a").map_status()?;
    let b = required_id(&r.b, "b").map_status()?;
    let bases = ctx.history.merge_bases(&a, &b).await.map_status()?;
    Ok(Response::new(v1::MergeBaseResponse {
        bases: ids_bytes(&bases),
    }))
}

/// `QueryService.Resolve` (SPEC 7.4, 11.8; implementation-defined 6.6): the changeset a
/// revision denotes. Requires repository read. INVALID_ARGUMENT/missing_field for an empty
/// revision, invalid_revision for a malformed one, ambiguous_prefix (with a Candidates payload)
/// for an ambiguous prefix; NOT_FOUND when nothing matches.
pub async fn resolve(
    ctx: &Arc<AppContext>,
    req: Request<v1::ResolveRequest>,
) -> Result<Response<v1::ResolveResponse>, Status> {
    let c = caller(&req)?;
    require_repo_read(ctx, &c.principal)?;
    let rev = &req.get_ref().revision;
    if rev.is_empty() {
        return Err(to_status(&missing_field("revision")));
    }
    let id = crate::revision::resolve_revision(&ctx.history, &ctx.derived, rev)
        .await
        .map_status()?;
    annotate(&req, |f| f.changeset_id = Some(id.to_text()));
    Ok(Response::new(v1::ResolveResponse {
        changeset: id_bytes(&id),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_carry_entry_first_and_final_last() {
        let e = EntryInfo {
            mode: Mode::File,
            id: ObjectId::hash_blob(b""),
            size: 0,
        };
        let c = read_path_chunks(&e, Bytes::new());
        assert_eq!(c.len(), 1);
        assert!(c[0].entry.is_some() && c[0].r#final && c[0].data.is_empty());

        let data = Bytes::from(vec![7u8; READ_PATH_CHUNK_BYTES * 2 + 5]);
        let c = read_path_chunks(&e, data.clone());
        assert_eq!(c.len(), 3);
        assert!(c[0].entry.is_some());
        assert!(c[1].entry.is_none() && c[2].entry.is_none());
        assert_eq!(
            c.iter().map(|x| x.r#final).collect::<Vec<_>>(),
            vec![false, false, true]
        );
        let joined: Vec<u8> = c.iter().flat_map(|x| x.data.clone()).collect();
        assert_eq!(joined, data.to_vec());
    }

    #[test]
    fn hunk_lines_are_prefixed() {
        let h = scion_merge::unified_hunks(b"a\nb\n", b"a\nc\n", DIFF_CONTEXT_LINES);
        let m = hunk_to_proto(h.into_iter().next().unwrap());
        assert_eq!(
            m.lines,
            vec![b" a\n".to_vec(), b"-b\n".to_vec(), b"+c\n".to_vec()]
        );
    }
}
