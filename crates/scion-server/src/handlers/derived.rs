//! Handlers: GetDerived (SPEC 9.1, 19.6) (P27).

use crate::app::AppContext;
use scion_proto::v1;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// `QueryService.GetDerived`.
pub async fn get_derived(
    ctx: &Arc<AppContext>,
    req: Request<v1::GetDerivedRequest>,
) -> Result<Response<v1::GetDerivedResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P27"))
}
