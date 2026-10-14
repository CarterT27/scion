//! Handlers: CheckAccess (SPEC 7.7, 10.4) (P25).

use crate::app::AppContext;
use scion_proto::v1;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// `AuthService.CheckAccess`.
pub async fn check_access(
    ctx: &Arc<AppContext>,
    req: Request<v1::CheckAccessRequest>,
) -> Result<Response<v1::CheckAccessResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P25"))
}
