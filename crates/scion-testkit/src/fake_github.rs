//! Fake GitHub API and OAuth device flow on loopback (PLAN 2.21, SPEC 19.0 "fake GitHub API";
//! P06).
//!
//! Endpoints (all JSON):
//!
//! - `GET /user` with `Authorization: Bearer <token>` (or `token <token>`): `{"login","id"}` for
//!   tokens registered with [`FakeGitHub::add_user`], otherwise 401.
//! - `POST /login/device/code` (JSON or form body with `client_id`): a fresh device code.
//! - `POST /login/oauth/access_token` (JSON or form body with `client_id`, `device_code`,
//!   `grant_type`): `{"error":"authorization_pending"}` until [`FakeGitHub::approve_device_code`],
//!   then `{"access_token","token_type","scope"}` once; afterwards the code is spent
//!   (`expired_token`). Knobs inject `slow_down`, `expired_token` and `access_denied`.
//!
//! While [`FakeGitHub::set_down`] is on, every endpoint returns 503.

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::task::AbortHandle;

#[derive(Default)]
struct DeviceGrant {
    device_code: String,
    user_code: String,
    token: Option<String>,
    expired: bool,
    denied: bool,
    spent: bool,
}

#[derive(Default)]
struct Inner {
    down: bool,
    users: BTreeMap<String, (String, u64)>,
    grants: Vec<DeviceGrant>,
    interval_secs: u64,
    expires_in_secs: u64,
    slow_down_next: u32,
    next_seq: u64,
    pending_polls: u64,
    user_requests: u64,
}

type Shared = Arc<Mutex<Inner>>;

/// axum on `127.0.0.1:0`: `GET /user`, the device-code endpoints and a down switch.
pub struct FakeGitHub {
    base: String,
    state: Shared,
    server: Option<AbortHandle>,
}

impl Drop for FakeGitHub {
    fn drop(&mut self) {
        if let Some(h) = &self.server {
            h.abort();
        }
    }
}

impl FakeGitHub {
    /// Start serving on `127.0.0.1:0`. Defaults: polling interval 0 s (clients apply their own
    /// floor), code lifetime 900 s. If loopback binding fails, the error is logged and the
    /// returned instance points at an unreachable address, so clients see UNAVAILABLE.
    pub async fn start() -> Self {
        let state: Shared = Arc::new(Mutex::new(Inner {
            expires_in_secs: 900,
            ..Inner::default()
        }));
        let app = Router::new()
            .route("/user", get(user))
            .route("/login/device/code", post(device_code))
            .route("/login/oauth/access_token", post(access_token))
            .fallback(fallback)
            .with_state(state.clone());
        let listener =
            match tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(error = %e, "FakeGitHub: cannot bind loopback");
                    return FakeGitHub {
                        base: "http://127.0.0.1:9".to_owned(),
                        state,
                        server: None,
                    };
                }
            };
        let base = match listener.local_addr() {
            Ok(a) => format!("http://{a}"),
            Err(_) => "http://127.0.0.1:9".to_owned(),
        };
        let task = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!(error = %e, "FakeGitHub: server stopped");
            }
        });
        FakeGitHub {
            base,
            state,
            server: Some(task.abort_handle()),
        }
    }

    /// Base URL of the REST API.
    pub fn api_url(&self) -> String {
        self.base.clone()
    }

    /// Base URL of the OAuth endpoints.
    pub fn oauth_base(&self) -> String {
        self.base.clone()
    }

    /// Register an access token for a user.
    pub fn add_user(&self, token: &str, login: &str, id: u64) {
        self.state
            .lock()
            .users
            .insert(token.to_owned(), (login.to_owned(), id));
    }

    /// Approve a pending device code, completing the flow with `token`. Unknown codes are
    /// ignored. The token is not implicitly registered for `GET /user`; use
    /// [`FakeGitHub::add_user`].
    pub fn approve_device_code(&self, user_code: &str, token: &str) {
        self.with_grant(user_code, |g| g.token = Some(token.to_owned()));
    }

    /// When down, every endpoint returns 503.
    pub fn set_down(&self, down: bool) {
        self.state.lock().down = down;
    }

    /// The `interval` (seconds) reported in device-code and `slow_down` responses (default 0).
    pub fn set_interval_secs(&self, secs: u64) {
        self.state.lock().interval_secs = secs;
    }

    /// The `expires_in` (seconds) reported in device-code responses (default 900).
    pub fn set_expires_in_secs(&self, secs: u64) {
        self.state.lock().expires_in_secs = secs;
    }

    /// The next `n` token polls of pending (unapproved) codes answer `slow_down`.
    pub fn slow_down_next(&self, n: u32) {
        self.state.lock().slow_down_next = n;
    }

    /// Make a device code answer `expired_token` from now on.
    pub fn expire_device_code(&self, user_code: &str) {
        self.with_grant(user_code, |g| g.expired = true);
    }

    /// Make a device code answer `access_denied` from now on.
    pub fn deny_device_code(&self, user_code: &str) {
        self.with_grant(user_code, |g| g.denied = true);
    }

    /// Number of token polls answered `authorization_pending` or `slow_down` so far.
    pub fn pending_polls(&self) -> u64 {
        self.state.lock().pending_polls
    }

    /// Number of `GET /user` requests served (including rejected and 503 ones).
    pub fn user_requests(&self) -> u64 {
        self.state.lock().user_requests
    }

    /// User codes issued so far, oldest first.
    pub fn user_codes(&self) -> Vec<String> {
        self.state
            .lock()
            .grants
            .iter()
            .map(|g| g.user_code.clone())
            .collect()
    }

    fn with_grant(&self, user_code: &str, f: impl FnOnce(&mut DeviceGrant)) {
        let mut s = self.state.lock();
        match s.grants.iter_mut().find(|g| g.user_code == user_code) {
            Some(g) => f(g),
            None => tracing::warn!("FakeGitHub: unknown user code"),
        }
    }
}

fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(json!({ "message": "Service Unavailable" })),
    )
        .into_response()
}

fn ok_json(v: Value) -> Response {
    (StatusCode::OK, axum::Json(v)).into_response()
}

async fn fallback(State(state): State<Shared>) -> Response {
    if state.lock().down {
        return unavailable();
    }
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({ "message": "Not Found" })),
    )
        .into_response()
}

async fn user(State(state): State<Shared>, headers: HeaderMap) -> Response {
    let mut s = state.lock();
    s.user_requests += 1;
    if s.down {
        return unavailable();
    }
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("token "))
        })
        .map(str::trim);
    match token.and_then(|t| s.users.get(t)) {
        Some((login, id)) => ok_json(json!({ "login": login, "id": id, "type": "User" })),
        None => (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({ "message": "Bad credentials" })),
        )
            .into_response(),
    }
}

/// Parse a JSON object or `application/x-www-form-urlencoded` body into string fields.
fn parse_fields(headers: &HeaderMap, body: &[u8]) -> BTreeMap<String, String> {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("json"));
    let mut out = BTreeMap::new();
    if is_json {
        if let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(body) {
            for (k, v) in m {
                if let Value::String(s) = v {
                    out.insert(k, s);
                }
            }
        }
        return out;
    }
    for pair in body.split(|&b| b == b'&').filter(|p| !p.is_empty()) {
        let mut it = pair.splitn(2, |&b| b == b'=');
        let k = percent_decode(it.next().unwrap_or_default());
        let v = percent_decode(it.next().unwrap_or_default());
        out.insert(k, v);
    }
    out
}

fn percent_decode(s: &[u8]) -> String {
    fn hex_val(b: u8) -> Option<u8> {
        (b as char).to_digit(16).map(|d| d as u8)
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while let Some(&b) = s.get(i) {
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                let hi = s.get(i + 1).copied().and_then(hex_val);
                let lo = s.get(i + 2).copied().and_then(hex_val);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push(hi << 4 | lo);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn device_code(State(state): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let fields = parse_fields(&headers, &body);
    let mut s = state.lock();
    if s.down {
        return unavailable();
    }
    if fields.get("client_id").is_none_or(|c| c.is_empty()) {
        return ok_json(json!({ "error": "unauthorized_client" }));
    }
    s.next_seq += 1;
    let seq = s.next_seq;
    let grant = DeviceGrant {
        device_code: format!("fake-device-code-{seq:08}"),
        user_code: format!("FAKE-{seq:04}"),
        ..DeviceGrant::default()
    };
    let resp = json!({
        "device_code": grant.device_code,
        "user_code": grant.user_code,
        "verification_uri": "https://github.com/login/device",
        "expires_in": s.expires_in_secs,
        "interval": s.interval_secs,
    });
    s.grants.push(grant);
    ok_json(resp)
}

async fn access_token(State(state): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let fields = parse_fields(&headers, &body);
    let mut s = state.lock();
    if s.down {
        return unavailable();
    }
    let err = |e: &str| ok_json(json!({ "error": e }));
    if fields.get("client_id").is_none_or(|c| c.is_empty()) {
        return err("unauthorized_client");
    }
    if fields.get("grant_type").map(String::as_str)
        != Some("urn:ietf:params:oauth:grant-type:device_code")
    {
        return err("unsupported_grant_type");
    }
    let Some(dc) = fields.get("device_code") else {
        return err("incorrect_device_code");
    };
    let interval = s.interval_secs;
    let slow = s.slow_down_next > 0;
    let Some(idx) = s.grants.iter().position(|g| &g.device_code == dc) else {
        return err("incorrect_device_code");
    };
    let g = &mut s.grants[idx];
    if g.spent || g.expired {
        return err("expired_token");
    }
    if g.denied {
        return err("access_denied");
    }
    if let Some(tok) = g.token.clone() {
        g.spent = true;
        return ok_json(json!({
            "access_token": tok,
            "token_type": "bearer",
            "scope": "",
        }));
    }
    s.pending_polls += 1;
    if slow {
        s.slow_down_next -= 1;
        return ok_json(json!({ "error": "slow_down", "interval": interval }));
    }
    err("authorization_pending")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_and_json_bodies() {
        let mut h = HeaderMap::new();
        let f = parse_fields(
            &h,
            b"client_id=abc&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&x=a+b&bad=%zz",
        );
        assert_eq!(f["client_id"], "abc");
        assert_eq!(
            f["grant_type"],
            "urn:ietf:params:oauth:grant-type:device_code"
        );
        assert_eq!(f["x"], "a b");
        assert_eq!(f["bad"], "%zz");
        h.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        let j = parse_fields(&h, br#"{"client_id":"abc","n":1}"#);
        assert_eq!(j.len(), 1);
        assert_eq!(j["client_id"], "abc");
    }
}
