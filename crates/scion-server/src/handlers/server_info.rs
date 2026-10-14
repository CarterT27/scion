//! Handlers: GetServerInfo (SPEC 7.3, 12.2) (P23).

use crate::app::AppContext;
use scion_proto::v1;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// The protocol packages this server speaks (SPEC 7.1).
pub const PROTOCOL_VERSIONS: [&str; 1] = ["scion.v1"];

/// `ObjectService.GetServerInfo` (SPEC 7.3): protocol versions, repository name, the current
/// policy and redaction epochs, the token verification keys, the configured limits and the
/// GitHub OAuth client id and base URL for the device flow (SPEC 12.2). Needs no token.
pub async fn get_server_info(
    ctx: &Arc<AppContext>,
    req: Request<v1::GetServerInfoRequest>,
) -> Result<Response<v1::ServerInfo>, Status> {
    let _ = req;
    let auth = &ctx.config.auth;
    Ok(Response::new(v1::ServerInfo {
        protocol_versions: PROTOCOL_VERSIONS.iter().map(|s| (*s).to_owned()).collect(),
        repo: ctx.config.server.repo.clone(),
        policy_epoch: ctx.policy.epoch(),
        redaction_epoch: ctx.redactions.epoch(),
        token_public_keys: ctx
            .keyring
            .public_keys()
            .iter()
            .map(|k| scion_proto::convert::public_key_to_proto(&k.kid, &k.ed25519_public))
            .collect(),
        limits: Some(v1::LimitsMsg::from(&ctx.config.limits)),
        github_client_id: auth.github_client_id.clone(),
        github_oauth_base_url: auth.github_oauth_base_url.clone(),
    }))
}
