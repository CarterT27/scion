//! Handlers: CreateChangeset, Rebase, Merge, Land (SPEC 7.5, 8) (P26).

use crate::app::AppContext;
use scion_proto::v1;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// `WriteService.CreateChangeset`.
pub async fn create_changeset(
    ctx: &Arc<AppContext>,
    req: Request<v1::CreateChangesetRequest>,
) -> Result<Response<v1::CreateChangesetResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P26"))
}

/// `WriteService.Rebase`.
pub async fn rebase(
    ctx: &Arc<AppContext>,
    req: Request<v1::RebaseRequest>,
) -> Result<Response<v1::RebaseResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P26"))
}

/// `WriteService.Merge`.
pub async fn merge(
    ctx: &Arc<AppContext>,
    req: Request<v1::MergeRequest>,
) -> Result<Response<v1::MergeResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P26"))
}

/// `WriteService.Land`.
pub async fn land(
    ctx: &Arc<AppContext>,
    req: Request<v1::LandRequest>,
) -> Result<Response<v1::LandResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P26"))
}
