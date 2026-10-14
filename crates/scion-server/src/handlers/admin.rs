//! Handlers: Redact, Unredact, PurgeNow, ListRedactions (SPEC 7.7, 13) (P25).

use crate::app::AppContext;
use scion_proto::v1;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// `AdminService.Redact`.
pub async fn redact(
    ctx: &Arc<AppContext>,
    req: Request<v1::RedactRequest>,
) -> Result<Response<v1::RedactResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P25"))
}

/// `AdminService.Unredact`.
pub async fn unredact(
    ctx: &Arc<AppContext>,
    req: Request<v1::UnredactRequest>,
) -> Result<Response<v1::UnredactResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P25"))
}

/// `AdminService.PurgeNow`.
pub async fn purge_now(
    ctx: &Arc<AppContext>,
    req: Request<v1::PurgeNowRequest>,
) -> Result<Response<v1::PurgeNowResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P25"))
}

/// `AdminService.ListRedactions`.
pub async fn list_redactions(
    ctx: &Arc<AppContext>,
    req: Request<v1::ListRedactionsRequest>,
) -> Result<Response<v1::ListRedactionsResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P25"))
}

/// Startup hook: starts the redaction mover and purge scheduler (P25). A no-op until then.
pub fn spawn_background(ctx: &Arc<AppContext>) {
    let _ = ctx;
}
