//! Handlers: GetChangesets, ReadPath, ListTree, Diff, IsAncestor, MergeBase, Resolve (SPEC 7.4) (P27).

use crate::app::AppContext;
use crate::handlers::BoxStream;
use scion_proto::v1;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// `QueryService.GetChangesets`.
pub async fn get_changesets(
    ctx: &Arc<AppContext>,
    req: Request<v1::GetChangesetsRequest>,
) -> Result<Response<v1::GetChangesetsResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P27"))
}

/// `QueryService.ReadPath`.
pub async fn read_path(
    ctx: &Arc<AppContext>,
    req: Request<v1::ReadPathRequest>,
) -> Result<Response<BoxStream<v1::ReadPathChunk>>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P27"))
}

/// `QueryService.ListTree`.
pub async fn list_tree(
    ctx: &Arc<AppContext>,
    req: Request<v1::ListTreeRequest>,
) -> Result<Response<v1::ListTreeResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P27"))
}

/// `QueryService.Diff`.
pub async fn diff(
    ctx: &Arc<AppContext>,
    req: Request<v1::DiffRequest>,
) -> Result<Response<BoxStream<v1::DiffItem>>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P27"))
}

/// `QueryService.IsAncestor`.
pub async fn is_ancestor(
    ctx: &Arc<AppContext>,
    req: Request<v1::IsAncestorRequest>,
) -> Result<Response<v1::IsAncestorResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P27"))
}

/// `QueryService.MergeBase`.
pub async fn merge_base(
    ctx: &Arc<AppContext>,
    req: Request<v1::MergeBaseRequest>,
) -> Result<Response<v1::MergeBaseResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P27"))
}

/// `QueryService.Resolve`.
pub async fn resolve(
    ctx: &Arc<AppContext>,
    req: Request<v1::ResolveRequest>,
) -> Result<Response<v1::ResolveResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P27"))
}
