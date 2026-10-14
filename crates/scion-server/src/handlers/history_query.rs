//! Handlers: Log and Blame (SPEC 7.4, 9.2, 12.6, 13.4) (P24).
//!
//! Read checks (SPEC 12.6, PLAN 6.12):
//!
//! - `Log` with a path and `Blame` require read on that path (PERMISSION_DENIED otherwise,
//!   checked before anything is looked up, so a denied caller learns nothing about the path).
//! - `Log` without a path requires repository read (read on `/`), like the other history
//!   queries.
//! - Every `Log` entry carries a `GetChangesets`-style item: the full canonical encoding when
//!   every delta path is readable by the caller, otherwise a `filtered_view` with the unreadable
//!   delta entries removed and `filtered = true`.
//! - The entry's `MetaView` is `redacted = true` with only `meta_id` set when the Meta is
//!   quarantined or purged (SPEC 13.4: clients render `<redacted>`); changeset ids are
//!   unchanged.

use crate::app::AppContext;
use crate::context::caller;
use crate::errors::{StatusExt, to_status};
use crate::logging::annotate;
use scion_core::{ErrorCode, ObjectId, Principal, ScionError};
use scion_derived::{blame as blame_type, log as path_log};
use scion_object::{Changeset, DeltaEntry, RepoPath};
use scion_policy::Policy;
use scion_proto::convert::{
    change_id_bytes, delta_to_proto, id_bytes, ids_bytes, missing_field, required_id,
};
use scion_proto::v1;
use scion_store::ChangesetRow;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// Page size of `Log` when the request leaves `limit` at 0 (SPEC 7.4).
pub const DEFAULT_LOG_LIMIT: usize = 100;
/// Largest `Log` page served; larger limits are clamped (SPEC 7.4).
pub const MAX_LOG_LIMIT: usize = 1000;

fn denied() -> Status {
    to_status(&ScionError::permission_denied(
        "read access to the requested path is denied",
    ))
}

/// The effective page size of a `Log` request (SPEC 7.4).
fn page_limit(requested: u32) -> usize {
    match requested {
        0 => DEFAULT_LOG_LIMIT,
        n => (n as usize).min(MAX_LOG_LIMIT),
    }
}

/// A path field: INVALID_ARGUMENT/invalid_path for a malformed path (SPEC 4.4).
fn parse_path(raw: &[u8]) -> Result<RepoPath, Status> {
    RepoPath::parse(raw).map_status()
}

/// `QueryService.Log` (SPEC 7.4, 12.6, 13.4).
///
/// - No `path`: `History::log_page` from `from` (requires read on `/`).
/// - With `path`: `scion_derived::log::log_path` (requires read on the path; an empty path is
///   the root). A path absent at `from` has an empty log.
///
/// `limit` 0 selects [`DEFAULT_LOG_LIMIT`]; larger limits are clamped to [`MAX_LOG_LIMIT`].
/// `page_token` continues a previous page of the same query; `next_page_token` is empty when
/// the walk is exhausted. Errors: NOT_FOUND for a missing `from`; INVALID_ARGUMENT for a
/// missing or malformed `from` (`missing_field` / `bad_id_text`), a malformed path or a bad page
/// token (`bad_page_token`); PERMISSION_DENIED as above.
pub async fn log(
    ctx: &Arc<AppContext>,
    req: Request<v1::LogRequest>,
) -> Result<Response<v1::LogResponse>, Status> {
    let who = caller(&req)?.principal;
    let r = req.get_ref();
    let from = required_id(&r.from, "from").map_status()?;
    annotate(&req, |f| f.changeset_id = Some(from.to_string()));
    let policy = ctx.policy.current();
    let limit = page_limit(r.limit);
    let token = (!r.page_token.is_empty()).then_some(r.page_token.as_slice());
    let (ids, next) = match &r.path {
        Some(raw) => {
            let path = parse_path(raw)?;
            if !policy.can_read(&who, &path) {
                return Err(denied());
            }
            let page = path_log::log_path(
                &ctx.derived,
                &from,
                &path,
                r.first_parent_only,
                limit,
                token,
            )
            .await
            .map_status()?;
            (page.changesets, page.next_page)
        }
        None => {
            if !policy.can_read(&who, &RepoPath::root()) {
                return Err(denied());
            }
            ctx.history
                .log_page(&from, r.first_parent_only, limit, token)
                .await
                .map_status()?
        }
    };
    let mut entries = Vec::with_capacity(ids.len());
    for id in &ids {
        entries.push(log_entry(ctx, &policy, &who, id).await?);
    }
    Ok(Response::new(v1::LogResponse {
        entries,
        next_page_token: next.unwrap_or_default(),
    }))
}

/// One `Log` entry: the changeset item and its Meta view.
async fn log_entry(
    ctx: &AppContext,
    policy: &Policy,
    who: &Principal,
    id: &ObjectId,
) -> Result<v1::LogEntry, Status> {
    let (cs, row) = ctx.objects.get_changeset(id).await.map_status()?;
    let item = changeset_item(policy, who, id, &cs, &row);
    let meta = meta_view(ctx, &cs.meta).await?;
    Ok(v1::LogEntry {
        id: id_bytes(id),
        changeset: Some(item),
        meta: Some(meta),
    })
}

/// SPEC 12.6: the full canonical encoding when every delta path is readable by `who`, else a
/// filtered view without the unreadable delta entries (`filtered = true`).
pub fn changeset_item(
    policy: &Policy,
    who: &Principal,
    id: &ObjectId,
    cs: &Changeset,
    row: &ChangesetRow,
) -> v1::ChangesetItem {
    let readable: Vec<&DeltaEntry> = cs
        .delta
        .iter()
        .filter(|e| policy.can_read(who, &e.path))
        .collect();
    let view = if readable.len() == cs.delta.len() {
        v1::changeset_item::View::Encoded(row.encoded.to_vec())
    } else {
        let kept: Vec<DeltaEntry> = readable.into_iter().cloned().collect();
        v1::changeset_item::View::FilteredView(v1::FilteredChangeset {
            parents: ids_bytes(&cs.parents),
            root_tree: id_bytes(&cs.root_tree),
            meta: id_bytes(&cs.meta),
            change_id: change_id_bytes(&cs.change_id),
            delta: delta_to_proto(&kept),
            filtered: true,
        })
    };
    v1::ChangesetItem {
        id: id_bytes(id),
        status: v1::ItemStatus::Ok as i32,
        generation: row.generation,
        view: Some(view),
    }
}

/// SPEC 13.4: the Meta as shown to clients; `redacted = true` with only `meta_id` when the
/// Meta is quarantined or purged (known to the redaction set or to storage).
async fn meta_view(ctx: &AppContext, meta_id: &ObjectId) -> Result<v1::MetaView, Status> {
    let redacted = v1::MetaView {
        redacted: true,
        meta_id: id_bytes(meta_id),
        ..v1::MetaView::default()
    };
    if ctx.redactions.contains(meta_id) {
        return Ok(redacted);
    }
    match ctx.objects.get_meta(meta_id).await {
        Ok(m) => Ok(v1::MetaView {
            redacted: false,
            author: m.author.clone(),
            author_time: Some(v1::Timestamp::from(&m.author_time)),
            committer: m.committer.clone(),
            committer_time: m.committer_time.as_ref().map(v1::Timestamp::from),
            creator: m.creator.clone(),
            message: m.message.clone(),
            meta_id: id_bytes(meta_id),
        }),
        Err(e) if e.code == ErrorCode::Redacted => Ok(redacted),
        Err(e) => Err(to_status(&e)),
    }
}

/// `QueryService.Blame` (SPEC 7.4, 9.2, 12.6): requires read on the path, then returns the
/// lazily derived blame ranges of the file at `changeset` (latest registered blame version).
///
/// Errors: INVALID_ARGUMENT for a missing or malformed `changeset` or `path`
/// (`missing_field` / `bad_id_text` / `invalid_path`); PERMISSION_DENIED when the caller cannot
/// read the path; NOT_FOUND for a missing changeset or a missing or non-file path.
pub async fn blame(
    ctx: &Arc<AppContext>,
    req: Request<v1::BlameRequest>,
) -> Result<Response<v1::BlameResponse>, Status> {
    let who = caller(&req)?.principal;
    let r = req.get_ref();
    let cs = required_id(&r.changeset, "changeset").map_status()?;
    annotate(&req, |f| f.changeset_id = Some(cs.to_string()));
    if r.path.is_empty() {
        return Err(to_status(&missing_field("path")));
    }
    let path = parse_path(&r.path)?;
    if !ctx.policy.current().can_read(&who, &path) {
        return Err(denied());
    }
    let ranges = blame_type::blame(&ctx.derived, &cs, &path, None)
        .await
        .map_status()?;
    Ok(Response::new(v1::BlameResponse {
        ranges: ranges
            .iter()
            .map(|b| v1::BlameRangeMsg {
                start_line: b.start_line,
                line_count: b.line_count,
                origin_changeset: id_bytes(&b.origin_changeset),
                origin_start_line: b.origin_start_line,
            })
            .collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits() {
        assert_eq!(page_limit(0), DEFAULT_LOG_LIMIT);
        assert_eq!(page_limit(5), 5);
        assert_eq!(page_limit(u32::MAX), MAX_LOG_LIMIT);
    }
}
