//! Handlers: GetObjects, HasObjects, ResolveRef (SPEC 7.3, 12.5) (P25).

use crate::app::AppContext;
use crate::handlers::BoxStream;
use scion_proto::v1;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// `ObjectService.GetObjects`.
pub async fn get_objects(
    ctx: &Arc<AppContext>,
    req: Request<v1::GetObjectsRequest>,
) -> Result<Response<BoxStream<v1::ObjectChunk>>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P25"))
}

/// `ObjectService.HasObjects`.
pub async fn has_objects(
    ctx: &Arc<AppContext>,
    req: Request<v1::HasObjectsRequest>,
) -> Result<Response<v1::HasObjectsResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P25"))
}

/// `ObjectService.ResolveRef`.
pub async fn resolve_ref(
    ctx: &Arc<AppContext>,
    req: Request<v1::ResolveRefRequest>,
) -> Result<Response<v1::ResolveRefResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P25"))
}
