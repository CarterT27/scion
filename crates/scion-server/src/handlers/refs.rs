//! Handlers: UpdateRef, CreateRef, DeleteRef, GetRefLog (SPEC 7.6) (P26).

use crate::app::AppContext;
use scion_proto::v1;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// `RefService.UpdateRef`.
pub async fn update_ref(
    ctx: &Arc<AppContext>,
    req: Request<v1::UpdateRefRequest>,
) -> Result<Response<v1::UpdateRefResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P26"))
}

/// `RefService.CreateRef`.
pub async fn create_ref(
    ctx: &Arc<AppContext>,
    req: Request<v1::CreateRefRequest>,
) -> Result<Response<v1::CreateRefResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P26"))
}

/// `RefService.DeleteRef`.
pub async fn delete_ref(
    ctx: &Arc<AppContext>,
    req: Request<v1::DeleteRefRequest>,
) -> Result<Response<v1::DeleteRefResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P26"))
}

/// `RefService.GetRefLog`.
pub async fn get_ref_log(
    ctx: &Arc<AppContext>,
    req: Request<v1::GetRefLogRequest>,
) -> Result<Response<v1::GetRefLogResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P26"))
}
