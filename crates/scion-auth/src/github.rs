//! GitHub REST client (`GET /user`, SPEC 12.2; P06).

use scion_core::{Principal, Result, ScionError, subcode};
use serde::Deserialize;
use std::time::Duration;

/// The authenticated GitHub user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubUser {
    /// Login.
    pub login: String,
    /// Stable numeric id.
    pub id: u64,
}

/// Calls the GitHub REST API.
#[derive(Clone)]
pub struct GitHubClient {
    api_url: String,
    http: Option<reqwest::Client>,
}

/// UNAVAILABLE for GitHub outages and transport failures (SPEC 16.1 class 10).
pub(crate) fn github_unavailable(what: &str) -> ScionError {
    ScionError::unavailable(
        subcode::UPSTREAM_UNAVAILABLE,
        format!("GitHub unavailable: {what}"),
    )
}

/// The HTTP client shared by the GitHub API and device-flow calls. `None` only if the TLS
/// backend cannot be initialised; callers then report UNAVAILABLE.
pub(crate) fn http_client() -> Option<reqwest::Client> {
    match reqwest::Client::builder()
        .user_agent(concat!("scion/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::error!(error = %e, "cannot build GitHub HTTP client");
            None
        }
    }
}

#[derive(Deserialize)]
struct UserResponse {
    login: String,
    id: u64,
}

impl GitHubClient {
    /// A client for `api_url` (e.g. `https://api.github.com`).
    pub fn new(api_url: &str) -> Self {
        GitHubClient {
            api_url: api_url.trim_end_matches('/').to_owned(),
            http: http_client(),
        }
    }

    /// `GET {api_url}/user` with `Authorization: Bearer` (SPEC 12.2 step 3). 401/403 (and other
    /// 4xx rejections) => UNAUTHENTICATED; transport errors, 429, 5xx and undecodable responses
    /// => UNAVAILABLE (SPEC 16.1 class 10). The access token is never logged.
    pub async fn user(&self, access_token: &str) -> Result<GitHubUser> {
        let http = self
            .http
            .as_ref()
            .ok_or_else(|| github_unavailable("no HTTP client"))?;
        let resp = http
            .get(format!("{}/user", self.api_url))
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(error = %e.without_url(), "GitHub GET /user failed");
                github_unavailable("transport error")
            })?;
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            tracing::warn!(status = status.as_u16(), "GitHub GET /user unavailable");
            return Err(github_unavailable(&format!("HTTP {}", status.as_u16())));
        }
        if status.is_client_error() {
            return Err(ScionError::unauthenticated(format!(
                "GitHub rejected the access token (HTTP {})",
                status.as_u16()
            )));
        }
        if !status.is_success() {
            return Err(github_unavailable(&format!(
                "unexpected HTTP {}",
                status.as_u16()
            )));
        }
        let user: UserResponse = resp
            .json()
            .await
            .map_err(|_| github_unavailable("malformed /user response"))?;
        // The login becomes the principal `github:<login>` (SPEC 12.2 step 3), so it must be a
        // valid principal name (implementation-defined limits: 1-255 printable ASCII bytes without `:` or `/`).
        if Principal::parse(&format!("github:{}", user.login)).is_err() {
            return Err(github_unavailable("invalid login in /user response"));
        }
        Ok(GitHubUser {
            login: user.login,
            id: user.id,
        })
    }
}
