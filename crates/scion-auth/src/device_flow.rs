//! GitHub OAuth device flow (SPEC 12.2 step 1; P06).
//!
//! `start` requests a device code; `poll` waits `interval` between token requests, handling
//! `authorization_pending` (keep polling), `slow_down` (adopt the returned interval, or add 5 s),
//! `expired_token` and `access_denied` (UNAUTHENTICATED). Transport errors, 429 and 5xx are
//! UNAVAILABLE (SPEC 16.1 class 10). Codes and tokens are never logged.

use crate::github::{github_unavailable, http_client};
use scion_core::{Result, ScionError};
use serde::Deserialize;
use std::time::Duration;

/// A pending device authorization.
#[derive(Clone, Debug)]
pub struct DeviceCode {
    /// Opaque device code.
    pub device_code: String,
    /// Code the user types.
    pub user_code: String,
    /// Where the user types it.
    pub verification_uri: String,
    /// Minimum polling interval.
    pub interval: Duration,
    /// Lifetime of the code.
    pub expires_in: Duration,
}

/// The OAuth grant type for device-code token requests (RFC 8628).
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Lower bound on the wait between polls, so a zero interval never busy-loops.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Added to the interval on `slow_down` when GitHub does not send a new interval (RFC 8628).
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct CodeResponse {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    interval: Option<u64>,
    expires_in: Option<u64>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    interval: Option<u64>,
}

async fn post_json<T: for<'de> Deserialize<'de>>(
    url: String,
    body: serde_json::Value,
    what: &str,
) -> Result<T> {
    let http = http_client().ok_or_else(|| github_unavailable("no HTTP client"))?;
    let resp = http
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(error = %e.without_url(), step = what, "GitHub device flow request failed");
            github_unavailable("transport error")
        })?;
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        tracing::warn!(
            status = status.as_u16(),
            step = what,
            "GitHub device flow unavailable"
        );
        return Err(github_unavailable(&format!("HTTP {}", status.as_u16())));
    }
    // GitHub reports OAuth errors as JSON `{"error": ...}`, usually with status 200 but
    // sometimes 4xx; both are decoded below.
    resp.json::<T>()
        .await
        .map_err(|_| github_unavailable(&format!("malformed {what} response")))
}

/// `POST {oauth_base}/login/device/code` (SPEC 12.2 step 1). An OAuth error (for example an
/// unknown client id or a disabled device flow) is UNAUTHENTICATED.
pub async fn start(oauth_base: &str, client_id: &str) -> Result<DeviceCode> {
    let base = oauth_base.trim_end_matches('/');
    let r: CodeResponse = post_json(
        format!("{base}/login/device/code"),
        serde_json::json!({ "client_id": client_id }),
        "device code",
    )
    .await?;
    if let Some(err) = r.error {
        return Err(ScionError::unauthenticated(format!(
            "GitHub refused the device flow: {err}"
        )));
    }
    match (r.device_code, r.user_code, r.verification_uri) {
        (Some(device_code), Some(user_code), Some(verification_uri))
            if !device_code.is_empty() && !user_code.is_empty() =>
        {
            Ok(DeviceCode {
                device_code,
                user_code,
                verification_uri,
                interval: Duration::from_secs(r.interval.unwrap_or(5)),
                expires_in: Duration::from_secs(r.expires_in.unwrap_or(900)),
            })
        }
        _ => Err(github_unavailable("incomplete device code response")),
    }
}

/// Poll `POST {oauth_base}/login/oauth/access_token` until approved, handling
/// `authorization_pending`, `slow_down` and `expired_token`; returns the GitHub access token
/// (SPEC 12.2 step 1). Denial, expiry (including running past `expires_in`) and other OAuth
/// errors are UNAUTHENTICATED.
pub async fn poll(oauth_base: &str, client_id: &str, dc: &DeviceCode) -> Result<String> {
    let base = oauth_base.trim_end_matches('/');
    let deadline = tokio::time::Instant::now() + dc.expires_in;
    let mut interval = dc.interval;
    loop {
        tokio::time::sleep(interval.max(MIN_POLL_INTERVAL)).await;
        if tokio::time::Instant::now() >= deadline {
            return Err(ScionError::unauthenticated("GitHub device code expired"));
        }
        let r: TokenResponse = post_json(
            format!("{base}/login/oauth/access_token"),
            serde_json::json!({
                "client_id": client_id,
                "device_code": dc.device_code,
                "grant_type": DEVICE_GRANT,
            }),
            "access token",
        )
        .await?;
        if let Some(tok) = r.access_token
            && !tok.is_empty()
        {
            return Ok(tok);
        }
        match r.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => {
                interval = match r.interval {
                    Some(s) => Duration::from_secs(s),
                    None => interval + SLOW_DOWN_STEP,
                };
                tracing::debug!(interval_secs = interval.as_secs(), "GitHub asked to slow down");
            }
            Some("expired_token") => {
                return Err(ScionError::unauthenticated("GitHub device code expired"));
            }
            Some("access_denied") => {
                return Err(ScionError::unauthenticated(
                    "GitHub device authorization was denied",
                ));
            }
            Some(other) => {
                return Err(ScionError::unauthenticated(format!(
                    "GitHub device flow failed: {other}"
                )));
            }
            None => return Err(github_unavailable("malformed access token response")),
        }
    }
}
