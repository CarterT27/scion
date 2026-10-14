//! `login` and `whoami` (SPEC 12.2, 12.3, 6.4, 17).

use super::CliIo;
use super::args::LoginArgs;
use super::ctx::{Ctx, ENV_SERVICE_SECRET, Reply, github_login};
use super::error::{CliResult, local};
use crate::output::views::{LoginData, WhoamiData};
use scion_auth::device_flow;
use scion_proto::{error_from_status, v1};
use scion_wc::credentials::{self, Credentials};

/// Default OAuth base when the server does not advertise one (SPEC 12.2).
const DEFAULT_OAUTH_BASE: &str = "https://github.com";

/// `scion login`: the GitHub device flow against the server's advertised client id
/// (SPEC 12.2), or `--service NAME` (SPEC 12.3). The credentials file is written with mode
/// 0600 and, for GitHub logins, keeps the GitHub token for silent re-exchange.
pub(crate) async fn login(
    ctx: &mut Ctx<'_>,
    io: &mut dyn CliIo,
    a: &LoginArgs,
) -> CliResult<Reply> {
    let root = ctx.find_root();
    let path = ctx.credentials_path()?;
    let (anon, endpoint) = ctx.connect_with(root.as_deref(), None).await?;
    let (resp, method, gh_token) = match &a.service {
        Some(name) => {
            let secret = match ctx.var(ENV_SERVICE_SECRET) {
                Some(s) => s,
                None => io.stdin_line().filter(|s| !s.is_empty()).ok_or_else(|| {
                    local("no service secret: set SCION_SERVICE_SECRET or pipe it on stdin")
                })?,
            };
            let resp = anon
                .auth()
                .login(v1::LoginRequest {
                    credential: Some(v1::login_request::Credential::Service(
                        v1::ServiceCredential {
                            service_name: name.clone(),
                            service_secret: secret,
                        },
                    )),
                })
                .await
                .map_err(|s| error_from_status(&s))?
                .into_inner();
            (resp, "service", None)
        }
        None => {
            let info = anon
                .objects()
                .get_server_info(v1::GetServerInfoRequest {})
                .await
                .map_err(|s| error_from_status(&s))?
                .into_inner();
            if info.github_client_id.is_empty() {
                return Err(local(format!(
                    "the server at {endpoint} does not advertise a GitHub client id; GitHub login is unavailable"
                )));
            }
            let oauth = if info.github_oauth_base_url.is_empty() {
                DEFAULT_OAUTH_BASE.to_owned()
            } else {
                info.github_oauth_base_url.clone()
            };
            let dc = device_flow::start(&oauth, &info.github_client_id).await?;
            let err = io.stderr();
            let _ = writeln!(
                err,
                "To log in, open {} and enter the code: {}",
                dc.verification_uri, dc.user_code
            );
            let _ = err.flush();
            let gh = device_flow::poll(&oauth, &info.github_client_id, &dc).await?;
            let resp = github_login(&anon, &gh).await?;
            (resp, "github", Some(gh))
        }
    };
    let store_gh = gh_token.is_some() && !a.no_store_github_token;
    let creds = Credentials {
        token: resp.token.clone(),
        github_token: if store_gh { gh_token } else { None },
    };
    credentials::write(&path, &creds)?;
    let data = LoginData {
        principal: resp.principal.clone(),
        expires_at_unix: resp.expires_at_unix,
        method: method.to_owned(),
        credentials_path: path.display().to_string(),
        github_token_stored: store_gh,
    };
    let human = format!(
        "Logged in as {} (token expires at {}); credentials saved to {}\n",
        resp.principal,
        super::util::fmt_time(&crate::output::views::TimeView {
            seconds: resp.expires_at_unix,
            tz_offset_minutes: 0
        }),
        path.display()
    );
    Reply::new("login", data, human)
}

/// `scion whoami` (SPEC 7.7).
pub(crate) async fn whoami(ctx: &mut Ctx<'_>) -> CliResult<Reply> {
    let root = ctx.find_root();
    let (client, endpoint) = ctx.client_and_endpoint(root.as_deref()).await?;
    let r = client
        .auth()
        .who_am_i(v1::WhoAmIRequest {})
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    let mut human = format!("{}\n", r.principal);
    if !r.groups.is_empty() {
        human.push_str(&format!("groups: {}\n", r.groups.join(", ")));
    }
    let data = WhoamiData {
        principal: r.principal,
        groups: r.groups,
        expires_at_unix: r.expires_at_unix,
        endpoint,
    };
    Reply::new("whoami", data, human)
}
