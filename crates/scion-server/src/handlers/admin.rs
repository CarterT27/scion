//! Handlers: Redact, Unredact, PurgeNow, ListRedactions (SPEC 7.7, 13) (P25).
//!
//! Authorization is checked here (`[admin] redact` / `purge`, SPEC 12.4); the mechanics, and
//! the SPEC 17 audit rows (written in the same transaction as each state change), are the
//! [`scion_redaction::Redactor`]'s, with the caller's principal as the actor. After a redact or
//! unredact the server refreshes its own redaction set at once, so it stops (or resumes)
//! serving the objects immediately rather than within the propagation bound (SPEC 13.3).

use crate::app::AppContext;
use crate::context::{CallerCtx, caller};
use crate::errors::{StatusExt, to_status};
use scion_core::{ObjectId, ScionError};
use scion_policy::AdminPerm;
use scion_proto::convert::{id_bytes, missing_field, parse_ids};
use scion_proto::v1;
use scion_store::RedactionState;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tonic::{Request, Response, Status};

/// The audit reason recorded by PurgeNow when the request carries none (SPEC 7.7 defines
/// `PurgeNow(object_ids)` without a reason).
pub const DEFAULT_PURGE_REASON: &str = "purge_now";

fn perm_name(a: AdminPerm) -> &'static str {
    match a {
        AdminPerm::Redact => "admin.redact",
        AdminPerm::Purge => "admin.purge",
        AdminPerm::Refs => "admin.refs",
    }
}

/// PERMISSION_DENIED unless the caller holds `perm` under the current policy (SPEC 12.4).
fn require_admin(ctx: &AppContext, c: &CallerCtx, perm: AdminPerm) -> Result<(), Status> {
    if ctx.policy.current().can_admin(&c.principal, perm) {
        return Ok(());
    }
    tracing::warn!(
        principal = %c.principal,
        permission = perm_name(perm),
        "admin RPC denied"
    );
    Err(to_status(&ScionError::permission_denied(format!(
        "{} requires {}",
        c.principal,
        perm_name(perm)
    ))))
}

/// Parse a non-empty id list (INVALID_ARGUMENT/missing_field when empty).
fn object_ids(v: &[Vec<u8>]) -> Result<Vec<ObjectId>, Status> {
    if v.is_empty() {
        return Err(to_status(&missing_field("object_ids")));
    }
    parse_ids(v, "object_ids").map_status()
}

fn unix_millis(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

/// Bring this server's redaction set up to `epoch` now (SPEC 13.3). A failed refresh is not
/// an error of the admin call: the refresher picks it up within the propagation bound.
async fn refresh_redactions(ctx: &AppContext, epoch: u64) {
    match ctx.redactions.refresh().await {
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, epoch, "redaction set refresh after admin call failed");
        }
    }
}

/// `AdminService.Redact` (SPEC 7.7, 13.2, 18.8; permission `admin.redact`): quarantine the
/// given Blob/Meta ids in one transaction that bumps the redaction epoch and records an audit
/// row (actor = caller, `reason`), then move their bytes to quarantine. `purge_after_seconds`
/// defaults to `redaction.default_quarantine`. Returns the new epoch and `effective_by` (Unix
/// ms) = commit time + propagation bound; this server stops serving the objects at once.
///
/// Errors: PERMISSION_DENIED; INVALID_ARGUMENT/missing_field (no ids or empty reason);
/// INVALID_ARGUMENT/not_redactable (tree or changeset ids); NOT_FOUND (unknown id);
/// UNAVAILABLE.
pub async fn redact(
    ctx: &Arc<AppContext>,
    req: Request<v1::RedactRequest>,
) -> Result<Response<v1::RedactResponse>, Status> {
    let c = caller(&req)?;
    require_admin(ctx, &c, AdminPerm::Redact)?;
    let r = req.into_inner();
    let ids = object_ids(&r.object_ids)?;
    if r.reason.trim().is_empty() {
        return Err(to_status(&missing_field("reason")));
    }
    let purge_after = r.purge_after_seconds.map(Duration::from_secs);
    let out = ctx
        .redactor
        .redact(&ids, &r.reason, &c.principal, purge_after)
        .await
        .map_status()?;
    refresh_redactions(ctx, out.epoch).await;
    Ok(Response::new(v1::RedactResponse {
        epoch: out.epoch,
        effective_by_unix_ms: unix_millis(out.effective_by),
    }))
}

/// `AdminService.Unredact` (SPEC 7.7, 13.2; permission `admin.redact`): restore quarantined
/// ids (bytes and state), bumping the epoch and recording an audit row (actor = caller).
/// Returns the new epoch; this server serves the objects again at once.
///
/// Errors: PERMISSION_DENIED; INVALID_ARGUMENT/missing_field (no ids);
/// INVALID_ARGUMENT/not_quarantined (live, unknown or purged ids; PLAN 6.7); UNAVAILABLE.
pub async fn unredact(
    ctx: &Arc<AppContext>,
    req: Request<v1::UnredactRequest>,
) -> Result<Response<v1::UnredactResponse>, Status> {
    let c = caller(&req)?;
    require_admin(ctx, &c, AdminPerm::Redact)?;
    let ids = object_ids(&req.get_ref().object_ids)?;
    let epoch = ctx
        .redactor
        .unredact(&ids, &c.principal)
        .await
        .map_status()?;
    refresh_redactions(ctx, epoch).await;
    Ok(Response::new(v1::UnredactResponse { epoch }))
}

/// `AdminService.PurgeNow` (SPEC 7.7, 13.2 step 4; permission `admin.purge`): irreversibly
/// purge quarantined ids now (bytes, inline data, content-bearing derived records), recording
/// an audit row per id with the caller as actor and `reason` (default
/// [`DEFAULT_PURGE_REASON`]). Already purged ids are skipped.
///
/// Errors: PERMISSION_DENIED; INVALID_ARGUMENT/missing_field (no ids);
/// INVALID_ARGUMENT/not_quarantined (live ids); NOT_FOUND (unknown ids); UNAVAILABLE.
pub async fn purge_now(
    ctx: &Arc<AppContext>,
    req: Request<v1::PurgeNowRequest>,
) -> Result<Response<v1::PurgeNowResponse>, Status> {
    let c = caller(&req)?;
    require_admin(ctx, &c, AdminPerm::Purge)?;
    let r = req.into_inner();
    let ids = object_ids(&r.object_ids)?;
    let reason = if r.reason.trim().is_empty() {
        DEFAULT_PURGE_REASON
    } else {
        r.reason.as_str()
    };
    ctx.redactor
        .purge_now(&ids, &c.principal, reason)
        .await
        .map_status()?;
    Ok(Response::new(v1::PurgeNowResponse {}))
}

/// The wire state name of a redaction (SPEC 13.2).
pub fn state_name(s: RedactionState) -> &'static str {
    match s {
        RedactionState::Quarantined => "quarantined",
        RedactionState::Purged => "purged",
    }
}

/// `AdminService.ListRedactions` (SPEC 7.7, 13.3; PLAN 6.12): redactions stamped with an epoch
/// greater than `since_epoch`, plus the current epoch. Any valid token may call it (daemons
/// poll it); it reveals ids, states and epochs only. The free-text `reason` is included only
/// for callers holding `admin.redact`.
pub async fn list_redactions(
    ctx: &Arc<AppContext>,
    req: Request<v1::ListRedactionsRequest>,
) -> Result<Response<v1::ListRedactionsResponse>, Status> {
    let c = caller(&req)?;
    let since = req.get_ref().since_epoch;
    let (rows, current) = ctx.redactor.list(since).await.map_status()?;
    ctx.redactions.observe(current);
    let show_reason = ctx
        .policy
        .current()
        .can_admin(&c.principal, AdminPerm::Redact);
    let entries = rows
        .iter()
        .map(|r| v1::RedactionEntry {
            object_id: id_bytes(&r.object),
            state: state_name(r.state).to_owned(),
            epoch: r.epoch,
            reason: if show_reason {
                r.reason.clone()
            } else {
                String::new()
            },
        })
        .collect();
    Ok(Response::new(v1::ListRedactionsResponse {
        entries,
        current_epoch: current,
    }))
}

/// Startup hook (SPEC 13.2): starts the redaction quarantine mover and purge scheduler. The
/// task stops when the server's context (which owns the redactor) is dropped.
pub fn spawn_background(ctx: &Arc<AppContext>) {
    let handles = crate::jobs::spawn(ctx);
    tracing::debug!(tasks = handles.len(), "background jobs started");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn millis_and_names() {
        assert_eq!(unix_millis(UNIX_EPOCH + Duration::from_millis(1500)), 1500);
        assert_eq!(unix_millis(UNIX_EPOCH - Duration::from_millis(5)), -5);
        assert_eq!(state_name(RedactionState::Purged), "purged");
        assert_eq!(perm_name(AdminPerm::Purge), "admin.purge");
    }
}
