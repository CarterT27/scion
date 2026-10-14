//! Handlers: Login and WhoAmI (SPEC 7.7, 12.1-12.3) (P23).

use crate::app::AppContext;
use crate::context::caller;
use crate::errors::{StatusExt, to_status};
use crate::logging::annotate;
use scion_core::{Principal, ScionError};
use scion_proto::v1;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tonic::{Request, Response, Status};

fn unix_seconds(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

/// `AuthService.Login` (SPEC 7.7, 12.2, 12.3). Needs no token.
///
/// - `github_access_token`: `GET /user` on the configured GitHub API; the principal is
///   `github:<login>` and the numeric id travels in the token's `gid` claim. A rejected
///   access token gives UNAUTHENTICATED; GitHub being down gives UNAVAILABLE (SPEC 16.1
///   class 10). The GitHub token is neither stored nor logged.
/// - `service`: SHA-256 of the secret compared in constant time against
///   `auth.service_principals`; a wrong secret or unknown name gives UNAUTHENTICATED.
///
/// Returns the Scion token, the principal id string and the expiry (Unix seconds).
pub async fn login(
    ctx: &Arc<AppContext>,
    req: Request<v1::LoginRequest>,
) -> Result<Response<v1::LoginResponse>, Status> {
    use v1::login_request::Credential;
    let principal = match &req.get_ref().credential {
        None => {
            return Err(to_status(&scion_proto::convert::missing_field(
                "credential",
            )));
        }
        Some(Credential::GithubAccessToken(token)) => {
            if token.is_empty() {
                return Err(to_status(&scion_proto::convert::missing_field(
                    "github_access_token",
                )));
            }
            let user = ctx.github.user(token).await.map_status()?;
            Principal::GitHub {
                login: user.login,
                id: Some(user.id),
            }
        }
        Some(Credential::Service(cred)) => {
            if !ctx
                .services
                .verify(&cred.service_name, cred.service_secret.as_bytes())
            {
                tracing::info!("service login rejected");
                return Err(to_status(&ScionError::unauthenticated(
                    "invalid service credentials",
                )));
            }
            Principal::parse(&format!("service:{}", cred.service_name)).map_err(|_| {
                to_status(&ScionError::unauthenticated("invalid service credentials"))
            })?
        }
    };
    let now = ctx.clock.now();
    let (token, claims) = ctx
        .keyring
        .issue(&principal, ctx.config.auth.token_ttl, now)
        .map_status()?;
    annotate(&req, |f| f.principal = Some(principal.id_string()));
    Ok(Response::new(v1::LoginResponse {
        token,
        principal: principal.id_string(),
        expires_at_unix: unix_seconds(claims.exp),
    }))
}

/// `AuthService.WhoAmI` (SPEC 7.7): the authenticated principal, its groups under the current
/// policy (sorted) and the token expiry (Unix seconds).
pub async fn who_am_i(
    ctx: &Arc<AppContext>,
    req: Request<v1::WhoAmIRequest>,
) -> Result<Response<v1::WhoAmIResponse>, Status> {
    let c = caller(&req)?;
    Ok(Response::new(v1::WhoAmIResponse {
        principal: c.principal.id_string(),
        groups: ctx.policy.current().groups_of(&c.principal),
        expires_at_unix: unix_seconds(c.claims.exp),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn unix_seconds_both_sides_of_epoch() {
        assert_eq!(unix_seconds(UNIX_EPOCH + Duration::from_secs(5)), 5);
        assert_eq!(unix_seconds(UNIX_EPOCH - Duration::from_secs(5)), -5);
    }
}
