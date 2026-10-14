//! Handlers: UpdateRef, CreateRef, DeleteRef, GetRefLog (SPEC 7.6, 8.5, 12.4, 17) (P26).
//!
//! Ref names are required (INVALID_ARGUMENT/missing_field when empty) and validated by
//! `RefName::parse` (INVALID_ARGUMENT/invalid_ref_name). `RefLogRow` / `LandedEntry` proto
//! conversions live here (amendment O-2).

use super::write::{landed_to_proto, log_slot, note, require_repo_read, required_ref};
use crate::app::AppContext;
use crate::authz::PolicyLandAuthzSource;
use crate::context::caller;
use crate::errors::{StatusExt, to_status};
use scion_core::{Principal, ScionError};
use scion_history::{LandAuthorizerSource, UpdateRefRequest};
use scion_policy::AdminPerm;
use scion_proto::convert::{opt_id_bytes, required_id};
use scion_proto::v1;
use scion_store::RefLogRow;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tonic::{Request, Response, Status};

/// GetRefLog page size when the request leaves `limit` at 0.
pub const DEFAULT_REF_LOG_LIMIT: usize = 1000;
/// Upper bound on one GetRefLog page.
pub const MAX_REF_LOG_LIMIT: usize = 10_000;

fn unix_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

/// `RefLogRow` to its proto form (SPEC 4.3.7; amendment O-2). Absent targets (creation's old
/// target, deletion's new target) are empty bytes.
pub fn ref_log_to_proto(r: &RefLogRow) -> v1::RefLogEntryMsg {
    v1::RefLogEntryMsg {
        version: r.version,
        old_target: opt_id_bytes(r.old_target.as_ref()),
        new_target: opt_id_bytes(r.new_target.as_ref()),
        principal: r.principal.clone(),
        at_unix_ms: unix_ms(r.at),
        landed: r.landed.iter().map(landed_to_proto).collect(),
    }
}

/// SPEC 7.6, 12.4: `admin.refs` under the current policy, else PERMISSION_DENIED.
fn require_admin_refs(ctx: &AppContext, who: &Principal) -> Result<(), Status> {
    if ctx.policy.current().can_admin(who, AdminPerm::Refs) {
        Ok(())
    } else {
        Err(to_status(&ScionError::permission_denied(
            "admin.refs permission required",
        )))
    }
}

/// `RefService.UpdateRef` (SPEC 7.6, 8.5).
///
/// `name` and `new_target` are required. Version-checked update through
/// `History::update_ref` with the current policy snapshot as `LandAuthorizer`: `update` on
/// the ref (PERMISSION_DENIED); REF_VERSION_MISMATCH carries the current version and target;
/// a non-fast-forward update needs `allow_non_fast_forward` AND `force` on the ref
/// (NOT_FAST_FORWARD otherwise) and is audited (SPEC 17). The `ref_log` row is written in the
/// same transaction.
pub async fn update_ref(
    ctx: &Arc<AppContext>,
    req: Request<v1::UpdateRefRequest>,
) -> Result<Response<v1::UpdateRefResponse>, Status> {
    let who = caller(&req)?.principal;
    let slot = log_slot(&req);
    let m = req.get_ref();
    let name = required_ref(&m.name, "name")?;
    let new_target = required_id(&m.new_target, "new_target").map_status()?;
    note(&slot, |f| {
        f.ref_name = Some(name.as_str().to_owned());
        f.changeset_id = Some(new_target.to_text());
    });
    let authz = PolicyLandAuthzSource::new(ctx.policy.clone(), who.clone()).current();
    let row = ctx
        .history
        .update_ref(
            UpdateRefRequest {
                name,
                expected_version: m.expected_version,
                new_target,
                allow_non_fast_forward: m.allow_non_fast_forward,
            },
            &who,
            authz.as_ref(),
        )
        .await
        .map_status()?;
    note(&slot, |f| f.ref_version = Some(row.version));
    Ok(Response::new(v1::UpdateRefResponse {
        version: row.version,
    }))
}

/// `RefService.CreateRef` (SPEC 7.6, 17; PLAN 6.7, 6.8).
///
/// Requires `admin.refs` (PERMISSION_DENIED). `name` and `target` are required; the target
/// must be an existing changeset (NOT_FOUND). An existing ref gives
/// INVALID_ARGUMENT/ref_exists. The ref, its `ref_log` row and an audit row carrying `reason`
/// are written in one transaction.
pub async fn create_ref(
    ctx: &Arc<AppContext>,
    req: Request<v1::CreateRefRequest>,
) -> Result<Response<v1::CreateRefResponse>, Status> {
    let who = caller(&req)?.principal;
    let slot = log_slot(&req);
    let m = req.get_ref();
    let name = required_ref(&m.name, "name")?;
    let target = required_id(&m.target, "target").map_status()?;
    note(&slot, |f| {
        f.ref_name = Some(name.as_str().to_owned());
        f.changeset_id = Some(target.to_text());
    });
    require_admin_refs(ctx, &who)?;
    let row = ctx
        .history
        .create_ref(&name, &target, &who, &m.reason)
        .await
        .map_status()?;
    note(&slot, |f| f.ref_version = Some(row.version));
    Ok(Response::new(v1::CreateRefResponse {
        version: row.version,
    }))
}

/// `RefService.DeleteRef` (SPEC 7.6, 17; PLAN 6.8).
///
/// Requires `admin.refs` (PERMISSION_DENIED). Deletes the ref at `expected_version`
/// (REF_VERSION_MISMATCH with payload otherwise; NOT_FOUND when absent), writing the
/// `ref_log` row and an audit row carrying `reason` in the same transaction.
pub async fn delete_ref(
    ctx: &Arc<AppContext>,
    req: Request<v1::DeleteRefRequest>,
) -> Result<Response<v1::DeleteRefResponse>, Status> {
    let who = caller(&req)?.principal;
    let slot = log_slot(&req);
    let m = req.get_ref();
    let name = required_ref(&m.name, "name")?;
    note(&slot, |f| {
        f.ref_name = Some(name.as_str().to_owned());
        f.ref_version = Some(m.expected_version);
    });
    require_admin_refs(ctx, &who)?;
    ctx.history
        .delete_ref(&name, m.expected_version, &who, &m.reason)
        .await
        .map_status()?;
    Ok(Response::new(v1::DeleteRefResponse {}))
}

/// `RefService.GetRefLog` (SPEC 7.6, 4.3.7).
///
/// Requires repository read (read on `/`). Returns the entries of `name` with version >=
/// `from_version`, ascending, at most `limit` (0 = [`DEFAULT_REF_LOG_LIMIT`], capped at
/// [`MAX_REF_LOG_LIMIT`]). A name without entries returns an empty list.
pub async fn get_ref_log(
    ctx: &Arc<AppContext>,
    req: Request<v1::GetRefLogRequest>,
) -> Result<Response<v1::GetRefLogResponse>, Status> {
    let who = caller(&req)?.principal;
    let slot = log_slot(&req);
    let m = req.get_ref();
    let name = required_ref(&m.name, "name")?;
    note(&slot, |f| f.ref_name = Some(name.as_str().to_owned()));
    require_repo_read(ctx, &who)?;
    let limit = match usize::try_from(m.limit).unwrap_or(MAX_REF_LOG_LIMIT) {
        0 => DEFAULT_REF_LOG_LIMIT,
        n => n.min(MAX_REF_LOG_LIMIT),
    };
    let rows = ctx
        .history
        .ref_log(&name, m.from_version, limit)
        .await
        .map_status()?;
    Ok(Response::new(v1::GetRefLogResponse {
        entries: rows.iter().map(ref_log_to_proto).collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::{ChangeId, Kind, ObjectId};
    use scion_object::RefName;
    use scion_store::LandedEntry;
    use std::time::Duration;

    #[test]
    fn ref_log_row_converts() {
        let a = ObjectId::new(Kind::Changeset, [1; 32]);
        let b = ObjectId::new(Kind::Changeset, [2; 32]);
        let row = RefLogRow {
            name: RefName::main(),
            version: 3,
            old_target: None,
            new_target: Some(b),
            principal: "github:alice".into(),
            at: UNIX_EPOCH + Duration::from_millis(1234),
            landed: vec![LandedEntry {
                change_id: ChangeId([7; 16]),
                source: a,
                result: b,
            }],
        };
        let m = ref_log_to_proto(&row);
        assert_eq!(m.version, 3);
        assert!(m.old_target.is_empty());
        assert_eq!(m.new_target, b.as_bytes().to_vec());
        assert_eq!(m.at_unix_ms, 1234);
        assert_eq!(m.principal, "github:alice");
        assert_eq!(m.landed.len(), 1);
        assert_eq!(m.landed[0].change_id, vec![7; 16]);
        assert_eq!(m.landed[0].source, a.as_bytes().to_vec());
        assert_eq!(unix_ms(UNIX_EPOCH - Duration::from_millis(5)), -5);
    }
}
