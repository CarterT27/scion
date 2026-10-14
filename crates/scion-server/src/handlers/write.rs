//! Handlers: CreateChangeset, Rebase, Merge, Land (SPEC 7.5, 8.1-8.4, 12.4) (P26).
//!
//! Every handler authenticates through [`caller`] (UNAUTHENTICATED without a valid token),
//! decodes the request with `scion_proto::convert` (INVALID_ARGUMENT/missing_field or
//! bad_id_text for absent or malformed fields) and delegates to `scion_history::History`, with
//! authorization adapters over the live policy:
//!
//! - CreateChangeset and Merge: [`PolicyWriteAuthz`] over the policy snapshot at request start
//!   (write on every delta path, SPEC 12.4; `import_mode` needs `admin.refs` + write on `/`,
//!   PLAN 6.4).
//! - Rebase (and Merge): repository read (read on `/`, PLAN 6.12). Rebase results are unnamed
//!   changesets, so rebasing someone else's changeset is allowed; landing checks write.
//! - Land: [`PolicyLandAuthzSource`], which re-reads the policy on every attempt (SPEC 8.4).
//!
//! MERGE_CONFLICT payloads omit conflicts and clean entries at paths the caller cannot read
//! (SPEC 12.6 applied to conflict reports; [`filter_conflict_payload`]).
//!
//! Error payloads (MERGE_CONFLICT `ConflictReport`, ALREADY_LANDED, TOO_LARGE limits, subcodes)
//! are encoded by `scion_proto::status_from_error` (SPEC 7.2). Log events carry
//! `changeset_id`, `ref`, `ref_version` and `land_attempt` where applicable (SPEC 15.1).

use crate::app::AppContext;
use crate::authz::{PolicyLandAuthzSource, PolicyWriteAuthz};
use crate::context::caller;
use crate::errors::{StatusExt, to_status};
use crate::logging::{LogFields, LogSlot};
use scion_core::{ErrorCode, ErrorPayload, ObjectId, Principal, ScionError};
use scion_history::{CreateRequest, LandAuthorizer, LandAuthorizerSource, LandRequest, MetaFields};
use scion_object::{RefName, RepoPath};
use scion_proto::convert::{
    self, CreateChangesetParts, MergeParts, MetaFieldsParts, change_id_bytes, id_bytes, required_id,
};
use scion_proto::v1;
use scion_store::LandedEntry;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tonic::{Request, Response, Status};

/// The log slot of the call `req` belongs to, kept so handlers can annotate the RPC event
/// after consuming the request (SPEC 15.1).
pub(crate) fn log_slot<T>(req: &Request<T>) -> Option<Arc<LogSlot>> {
    req.extensions().get::<Arc<LogSlot>>().cloned()
}

/// Add SPEC 15.1 fields through a slot obtained with [`log_slot`] (no-op without one).
pub(crate) fn note(slot: &Option<Arc<LogSlot>>, f: impl FnOnce(&mut LogFields)) {
    if let Some(s) = slot {
        s.update(f);
    }
}

/// SPEC 12.4, PLAN 6.12: repository read (read on `/`) under the current policy, else
/// PERMISSION_DENIED.
pub(crate) fn require_repo_read(ctx: &AppContext, who: &Principal) -> Result<(), Status> {
    if ctx.policy.current().can_read(who, &RepoPath::root()) {
        Ok(())
    } else {
        Err(to_status(&ScionError::permission_denied(
            "repository read (read on /) denied",
        )))
    }
}

/// SPEC 12.6 applied to MERGE_CONFLICT payloads (Rebase, Merge, Land): conflicts and clean
/// entries at paths the caller cannot read under the current policy are omitted, so a
/// repository reader rebasing someone else's changeset never learns restricted path names or
/// object ids. Other errors pass through unchanged. When entries are removed the message is
/// rewritten so it does not reveal the unfiltered conflict count.
pub(crate) fn filter_conflict_payload(
    ctx: &AppContext,
    who: &Principal,
    mut e: ScionError,
) -> ScionError {
    if e.code != ErrorCode::MergeConflict {
        return e;
    }
    let ErrorPayload::MergeConflict(report) = &mut e.payload else {
        return e;
    };
    let policy = ctx.policy.current();
    let (conflicts, clean) = (report.conflicts.len(), report.clean_entries.len());
    report
        .conflicts
        .retain(|c| policy.can_read_bytes(who, &c.path));
    report
        .clean_entries
        .retain(|c| policy.can_read_bytes(who, &c.path));
    if report.conflicts.len() != conflicts || report.clean_entries.len() != clean {
        let shown = report.conflicts.len();
        tracing::debug!(
            omitted_conflicts = conflicts - shown,
            omitted_clean = clean - report.clean_entries.len(),
            "merge conflict payload filtered by read policy"
        );
        e.message = format!("merge conflict: {shown} readable conflicting path(s)");
    }
    e
}

/// Parse a required ref name: empty is INVALID_ARGUMENT/missing_field, otherwise
/// `RefName::parse` (INVALID_ARGUMENT/invalid_ref_name; SPEC 4.3.6).
pub(crate) fn required_ref(name: &str, field: &str) -> Result<RefName, Status> {
    if name.is_empty() {
        return Err(to_status(&convert::missing_field(field)));
    }
    RefName::parse(name).map_status()
}

/// `LandedEntry` to its proto form (amendment O-2: store-row conversions live here).
pub(crate) fn landed_to_proto(e: &LandedEntry) -> v1::LandedEntry {
    v1::LandedEntry {
        change_id: change_id_bytes(&e.change_id),
        source: id_bytes(&e.source),
        result: id_bytes(&e.result),
    }
}

/// `v1::MetaFields` (decoded) to the history request type (SPEC 7.5; no `creator`: the
/// server sets it, SPEC 4.3.3).
fn meta_fields(m: MetaFieldsParts) -> MetaFields {
    MetaFields {
        author: m.author,
        author_time: m.author_time,
        committer: m.committer,
        committer_time: m.committer_time,
        message: m.message,
        extensions: m.extensions,
    }
}

/// `WriteService.CreateChangeset` (SPEC 7.5, 8.1, 18.1).
///
/// `meta_fields` and `change_id` are required (INVALID_ARGUMENT/missing_field). The server
/// builds the trees and the Meta object, setting `creator` to the authenticated principal; the
/// request has no way to supply one. Validation follows SPEC 8.1 in order (subcodes,
/// TOO_LARGE, REDACTED, PERMISSION_DENIED for unwritable delta paths); `expected_root_tree`
/// mismatch is INVALID_ARGUMENT/root_tree_mismatch. `import_mode` requires `admin.refs` and
/// write on `/` (PLAN 6.4; PERMISSION_DENIED otherwise) and permits parentless changesets.
/// Identical requests return the same identifiers.
pub async fn create_changeset(
    ctx: &Arc<AppContext>,
    req: Request<v1::CreateChangesetRequest>,
) -> Result<Response<v1::CreateChangesetResponse>, Status> {
    let who = caller(&req)?.principal;
    let slot = log_slot(&req);
    let parts = CreateChangesetParts::try_from(req.get_ref()).map_status()?;
    let authz = PolicyWriteAuthz::new(ctx.policy.current(), who.clone());
    let created = ctx
        .history
        .create_changeset(
            CreateRequest {
                parents: parts.parents,
                delta: parts.delta,
                meta: meta_fields(parts.meta),
                change_id: parts.change_id,
                expected_root_tree: parts.expected_root_tree,
                import_mode: parts.import_mode,
            },
            &who,
            &authz,
        )
        .await
        .map_status()?;
    note(&slot, |f| {
        f.changeset_id = Some(created.changeset_id.to_text())
    });
    Ok(Response::new(v1::CreateChangesetResponse {
        changeset_id: id_bytes(&created.changeset_id),
        root_tree: id_bytes(&created.root_tree),
        meta_id: id_bytes(&created.meta_id),
    }))
}

/// `WriteService.Rebase` (SPEC 7.5, 8.2, 18.3).
///
/// Requires repository read only (PLAN 6.12): the result is an unnamed changeset and landing
/// it checks write, so any reader may rebase anyone's changeset. Deterministic: the same
/// `(changeset, onto)` yields the same id on every server and after restarts. Errors:
/// MERGE_CONFLICT (payload with `clean_entries` and conflicts), EMPTY_CHANGE,
/// INVALID_ARGUMENT/multi_parent, NOT_FOUND.
pub async fn rebase(
    ctx: &Arc<AppContext>,
    req: Request<v1::RebaseRequest>,
) -> Result<Response<v1::RebaseResponse>, Status> {
    let who = caller(&req)?.principal;
    let slot = log_slot(&req);
    let m = req.get_ref();
    let c = required_id(&m.changeset, "changeset").map_status()?;
    let onto = required_id(&m.onto, "onto").map_status()?;
    require_repo_read(ctx, &who)?;
    note(&slot, |f| f.changeset_id = Some(c.to_text()));
    let id = ctx
        .history
        .rebase(&c, &onto, &who)
        .await
        .map_err(|e| filter_conflict_payload(ctx, &who, e))
        .map_status()?;
    note(&slot, |f| f.changeset_id = Some(id.to_text()));
    Ok(Response::new(v1::RebaseResponse {
        changeset_id: id_bytes(&id),
    }))
}

/// `WriteService.Merge` (SPEC 7.5, 8.3).
///
/// Every field is required (INVALID_ARGUMENT/missing_field). Requires repository read (the
/// merge reads both sides and conflict payloads name their paths) and write on every path of
/// the resulting delta against `a` ([`PolicyWriteAuthz`]; PERMISSION_DENIED). Errors:
/// MERGE_CONFLICT (payload), AMBIGUOUS_MERGE_BASE, NOT_FOUND, TOO_LARGE.
pub async fn merge(
    ctx: &Arc<AppContext>,
    req: Request<v1::MergeRequest>,
) -> Result<Response<v1::MergeResponse>, Status> {
    let who = caller(&req)?.principal;
    let slot = log_slot(&req);
    let parts = MergeParts::try_from(req.get_ref()).map_status()?;
    require_repo_read(ctx, &who)?;
    let authz = PolicyWriteAuthz::new(ctx.policy.current(), who.clone());
    let id = ctx
        .history
        .merge(
            &parts.a,
            &parts.b,
            meta_fields(parts.meta),
            parts.change_id,
            &who,
            &authz,
        )
        .await
        .map_err(|e| filter_conflict_payload(ctx, &who, e))
        .map_status()?;
    note(&slot, |f| f.changeset_id = Some(id.to_text()));
    Ok(Response::new(v1::MergeResponse {
        changeset_id: id_bytes(&id),
    }))
}

/// A [`LandAuthorizerSource`] that counts attempts: History calls `current()` exactly once in
/// `Resolving` of every attempt (SPEC 8.4), so the count is the attempt number for the log.
struct CountingSource {
    inner: PolicyLandAuthzSource,
    attempts: AtomicU32,
}

impl LandAuthorizerSource for CountingSource {
    fn current(&self) -> Arc<dyn LandAuthorizer> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        self.inner.current()
    }
}

/// `WriteService.Land` (SPEC 7.5, 8.4, 18.4).
///
/// `ref` and `head` are required. Lock-free land through `History::land` with
/// [`PolicyLandAuthzSource`]: `update` on the ref and write on every delta path of every
/// rebased member are checked against the live policy on every attempt. Errors:
/// MERGE_CONFLICT, EMPTY_CHANGE (unless `allow_empty`), ALREADY_LANDED (payload with the
/// recorded source, result and version), INVALID_ARGUMENT/merge_in_stack, PERMISSION_DENIED,
/// NOT_FOUND, UNAVAILABLE/land_contention. A retry of a landed stack returns the recorded
/// result. Land metrics come from the `LandObserver` wired into History (SPEC 15.2); the RPC
/// event carries `land_attempt`, `ref`, `ref_version` and `changeset_id` (SPEC 15.1).
pub async fn land(
    ctx: &Arc<AppContext>,
    req: Request<v1::LandRequest>,
) -> Result<Response<v1::LandResponse>, Status> {
    let who = caller(&req)?.principal;
    let slot = log_slot(&req);
    let m = req.get_ref();
    let ref_name = required_ref(&m.r#ref, "ref")?;
    let head: ObjectId = required_id(&m.head, "head").map_status()?;
    note(&slot, |f| {
        f.ref_name = Some(ref_name.as_str().to_owned());
        f.changeset_id = Some(head.to_text());
    });
    let source = CountingSource {
        inner: PolicyLandAuthzSource::new(ctx.policy.clone(), who.clone()),
        attempts: AtomicU32::new(0),
    };
    let result = ctx
        .history
        .land(
            LandRequest {
                ref_name,
                head,
                allow_empty: m.allow_empty,
            },
            &who,
            &source,
        )
        .await;
    let attempts = source.attempts.load(Ordering::Relaxed);
    note(&slot, |f| f.land_attempt = Some(attempts.max(1)));
    let result = result
        .map_err(|e| filter_conflict_payload(ctx, &who, e))
        .map_status()?;
    note(&slot, |f| {
        f.ref_version = Some(result.version);
        f.changeset_id = Some(result.target.to_text());
    });
    Ok(Response::new(v1::LandResponse {
        target: id_bytes(&result.target),
        version: result.version,
        landed: result.landed.iter().map(landed_to_proto).collect(),
    }))
}
