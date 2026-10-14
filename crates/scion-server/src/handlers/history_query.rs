//! Handlers: Log and Blame (SPEC 7.4, 9.2) (P24).

use crate::app::AppContext;
use scion_proto::v1;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// `QueryService.Log`.
pub async fn log(
    ctx: &Arc<AppContext>,
    req: Request<v1::LogRequest>,
) -> Result<Response<v1::LogResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P24"))
}

/// `QueryService.Blame`.
pub async fn blame(
    ctx: &Arc<AppContext>,
    req: Request<v1::BlameRequest>,
) -> Result<Response<v1::BlameResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P24"))
}
