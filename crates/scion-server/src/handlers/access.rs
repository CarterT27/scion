//! Handlers: CheckAccess (SPEC 7.7, 10.4) (P25).

use crate::app::AppContext;
use crate::authz::ReadDecision;
use crate::context::caller;
use crate::errors::{StatusExt, to_status};
use crate::handlers::object::access_class_to_proto;
use scion_proto::convert::{missing_field, parse_ids};
use scion_proto::v1;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// `AuthService.CheckAccess` (SPEC 7.7, 10.4; PLAN 6.12): object read decisions for the
/// principal of `subject_token` (used by daemons on behalf of their callers).
///
/// The caller needs any valid token (checked by the interceptor). The subject token is verified
/// with the server's keys (UNAUTHENTICATED when invalid or expired; INVALID_ARGUMENT/
/// missing_field when empty). For each id, `allowed[i]` is the SPEC 18.6 decision for the
/// subject and `access_class[i]` its class (`ROOT` / `RESTRICTED`) when allowed, otherwise
/// `ACCESS_CLASS_UNSPECIFIED` (denied, absent and redacted objects look the same). Upload
/// records do not grant reads. Malformed ids are INVALID_ARGUMENT.
pub async fn check_access(
    ctx: &Arc<AppContext>,
    req: Request<v1::CheckAccessRequest>,
) -> Result<Response<v1::CheckAccessResponse>, Status> {
    let _c = caller(&req)?;
    let r = req.into_inner();
    if r.subject_token.is_empty() {
        return Err(to_status(&missing_field("subject_token")));
    }
    let subject = ctx
        .verifier
        .verify(&r.subject_token, ctx.clock.now())
        .map_status()?
        .sub;
    let ids = parse_ids(&r.object_ids, "object_ids").map_status()?;
    let mut allowed = Vec::with_capacity(ids.len());
    let mut access_class = Vec::with_capacity(ids.len());
    for id in &ids {
        match ctx.authz.can_read(&subject, id).await.map_status()? {
            ReadDecision::Allow(c) => {
                allowed.push(true);
                access_class.push(access_class_to_proto(c) as i32);
            }
            ReadDecision::Deny | ReadDecision::Redacted => {
                allowed.push(false);
                access_class.push(v1::AccessClass::Unspecified as i32);
            }
        }
    }
    Ok(Response::new(v1::CheckAccessResponse {
        allowed,
        access_class,
    }))
}
