//! GitHub REST client (`GET /user`, SPEC 12.2; P06).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use scion_core::Result;

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
}

impl GitHubClient {
    /// A client for `api_url` (e.g. `https://api.github.com`).
    pub fn new(api_url: &str) -> Self {
        GitHubClient {
            api_url: api_url.trim_end_matches('/').to_owned(),
        }
    }

    /// `GET {api_url}/user` with `Authorization: Bearer`. 401/403 => UNAUTHENTICATED;
    /// transport errors and 5xx => UNAVAILABLE.
    pub async fn user(&self, access_token: &str) -> Result<GitHubUser> {
        todo!("P06")
    }
}
