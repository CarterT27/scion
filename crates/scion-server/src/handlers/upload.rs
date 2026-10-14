//! Handlers: UploadObjects (SPEC 7.5) (P25).

use crate::app::AppContext;
use scion_proto::v1;
use std::sync::Arc;
use tonic::Streaming;
use tonic::{Request, Response, Status};

/// `WriteService.UploadObjects`.
pub async fn upload_objects(
    ctx: &Arc<AppContext>,
    req: Request<Streaming<v1::UploadItem>>,
) -> Result<Response<v1::UploadObjectsResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P25"))
}
