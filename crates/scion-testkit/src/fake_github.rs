//! Fake GitHub API and OAuth device flow on loopback (PLAN 2.21; P06).
#![allow(unused_variables)] // P00 stub: remove when implemented.

/// axum on `127.0.0.1:0`: `GET /user`, the device-code endpoints and a down switch.
pub struct FakeGitHub {
    _private: (),
}

impl FakeGitHub {
    /// Start serving.
    pub async fn start() -> Self {
        todo!("P06")
    }

    /// Base URL of the REST API.
    pub fn api_url(&self) -> String {
        todo!("P06")
    }

    /// Base URL of the OAuth endpoints.
    pub fn oauth_base(&self) -> String {
        todo!("P06")
    }

    /// Register an access token for a user.
    pub fn add_user(&self, token: &str, login: &str, id: u64) {
        todo!("P06")
    }

    /// Approve a pending device code, completing the flow with `token`.
    pub fn approve_device_code(&self, user_code: &str, token: &str) {
        todo!("P06")
    }

    /// When down, every endpoint returns 503.
    pub fn set_down(&self, down: bool) {
        todo!("P06")
    }
}
