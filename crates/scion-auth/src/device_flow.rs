//! GitHub OAuth device flow (SPEC 12.2; P06).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use scion_core::Result;
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

/// `POST {oauth_base}/login/device/code`.
pub async fn start(oauth_base: &str, client_id: &str) -> Result<DeviceCode> {
    todo!("P06")
}

/// Poll `POST {oauth_base}/login/oauth/access_token` until approved, handling
/// `authorization_pending`, `slow_down` and `expired_token`; returns the GitHub access token.
pub async fn poll(oauth_base: &str, client_id: &str, dc: &DeviceCode) -> Result<String> {
    todo!("P06")
}
