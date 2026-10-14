//! Handlers: Login and WhoAmI (SPEC 7.7, 12) (P23).

use crate::app::AppContext;
use scion_proto::v1;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// `AuthService.Login`.
pub async fn login(
    ctx: &Arc<AppContext>,
    req: Request<v1::LoginRequest>,
) -> Result<Response<v1::LoginResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P23"))
}

/// `AuthService.WhoAmI`.
pub async fn who_am_i(
    ctx: &Arc<AppContext>,
    req: Request<v1::WhoAmIRequest>,
) -> Result<Response<v1::WhoAmIResponse>, Status> {
    let _ = (ctx, req);
    Err(Status::unimplemented("P23"))
}
