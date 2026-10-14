//! Handlers: GetServerInfo (SPEC 7.3) (P23).

use crate::app::AppContext;
use scion_proto::v1;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// `ObjectService.GetServerInfo`.
pub async fn get_server_info(
    ctx: &Arc<AppContext>,
    req: Request<v1::GetServerInfoRequest>,
) -> Result<Response<v1::ServerInfo>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P23"))
}
