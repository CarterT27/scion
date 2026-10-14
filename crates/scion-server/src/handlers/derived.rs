//! Handlers: GetDerived (SPEC 9.1, 19.6) (P27).
//!
//! The generic derived-data query path: any type registered in the server's
//! [`scion_derived::Registry`] is queryable by name, optional version and key, so adding a type
//! or bumping a version needs no client change (SPEC 9.1).

use crate::app::AppContext;
use crate::context::caller;
use crate::errors::{StatusExt, to_status};
use scion_core::ScionError;
use scion_object::RepoPath;
use scion_proto::convert::missing_field;
use scion_proto::v1;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// `QueryService.GetDerived` (SPEC 9.1, 19.6; implementation-defined 6.12).
///
/// Requires repository read (read on `/`) for every type, content-bearing or not. Returns the
/// value of `(type, version, key)` through [`scion_derived::DerivedEngine::get`] (lazy types
/// are derived on demand and written back) and the version served: the requested one, or the
/// latest registered version when none is given. Old versions stay readable (SPEC 9.1).
/// INVALID_ARGUMENT/missing_field for an empty `type`; NOT_FOUND for an unregistered type or a
/// key with no value.
pub async fn get_derived(
    ctx: &Arc<AppContext>,
    req: Request<v1::GetDerivedRequest>,
) -> Result<Response<v1::GetDerivedResponse>, Status> {
    let c = caller(&req)?;
    if !ctx
        .policy
        .current()
        .can_read(&c.principal, &RepoPath::root())
    {
        return Err(to_status(&ScionError::permission_denied(
            "read access denied: repository read (read on /) required",
        )));
    }
    let r = req.get_ref();
    if r.r#type.is_empty() {
        return Err(to_status(&missing_field("type")));
    }
    if !ctx.derived.registry().contains(&r.r#type) {
        return Err(to_status(&ScionError::not_found(format!(
            "unknown derived type {:?}",
            r.r#type
        ))));
    }
    let version = ctx
        .derived
        .resolve_version(&r.r#type, r.version)
        .map_status()?;
    let value = ctx
        .derived
        .get(&r.r#type, Some(version), &r.key)
        .await
        .map_status()?;
    Ok(Response::new(v1::GetDerivedResponse {
        value: value.to_vec(),
        version,
    }))
}
