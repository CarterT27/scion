//! FakeGitHub over raw HTTP/1.1 (no HTTP client dependency in scion-testkit; the reqwest-based
//! client paths are exercised by scion-auth's tests). PLAN 2.21, SPEC 19.0.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only helpers outside #[test] fns

use scion_testkit::FakeGitHub;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Sends one request with `Connection: close` and returns (status, body).
async fn request(
    base: &str,
    method: &str,
    path: &str,
    headers: &[&str],
    body: &str,
) -> (u16, String) {
    let addr = base.strip_prefix("http://").unwrap();
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for h in headers {
        req.push_str(h);
        req.push_str("\r\n");
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    let status: u16 = text.split(' ').nth(1).unwrap().parse().unwrap();
    let body = text.split_once("\r\n\r\n").unwrap().1.to_owned();
    (status, body)
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap()
}

#[tokio::test]
async fn user_endpoint_and_down_switch() {
    let gh = FakeGitHub::start().await;
    assert_eq!(gh.api_url(), gh.oauth_base());
    gh.add_user("tok-alice", "alice", 4242);

    let (st, body) = request(
        &gh.api_url(),
        "GET",
        "/user",
        &["Authorization: Bearer tok-alice"],
        "",
    )
    .await;
    assert_eq!(st, 200);
    let v = json(&body);
    assert_eq!(v["login"], "alice");
    assert_eq!(v["id"], 4242);

    let (st, _) = request(
        &gh.api_url(),
        "GET",
        "/user",
        &["Authorization: token tok-alice"],
        "",
    )
    .await;
    assert_eq!(st, 200);
    let (st, _) = request(
        &gh.api_url(),
        "GET",
        "/user",
        &["Authorization: Bearer nope"],
        "",
    )
    .await;
    assert_eq!(st, 401);
    let (st, _) = request(&gh.api_url(), "GET", "/user", &[], "").await;
    assert_eq!(st, 401);

    gh.set_down(true);
    let (st, _) = request(
        &gh.api_url(),
        "GET",
        "/user",
        &["Authorization: Bearer tok-alice"],
        "",
    )
    .await;
    assert_eq!(st, 503);
    let (st, _) = request(
        &gh.oauth_base(),
        "POST",
        "/login/device/code",
        &[],
        "client_id=x",
    )
    .await;
    assert_eq!(st, 503);
    let (st, _) = request(&gh.api_url(), "GET", "/nope", &[], "").await;
    assert_eq!(st, 503);
    gh.set_down(false);
    let (st, _) = request(&gh.api_url(), "GET", "/nope", &[], "").await;
    assert_eq!(st, 404);
    assert_eq!(gh.user_requests(), 5);
}

#[tokio::test]
async fn device_flow_form_encoded() {
    let gh = FakeGitHub::start().await;
    gh.set_interval_secs(3);
    let form = [
        "Content-Type: application/x-www-form-urlencoded",
        "Accept: application/json",
    ];
    let (st, body) = request(
        &gh.oauth_base(),
        "POST",
        "/login/device/code",
        &form,
        "client_id=cid&scope=",
    )
    .await;
    assert_eq!(st, 200);
    let code = json(&body);
    assert_eq!(code["interval"], 3);
    assert_eq!(code["expires_in"], 900);
    let user_code = code["user_code"].as_str().unwrap().to_owned();
    let device_code = code["device_code"].as_str().unwrap().to_owned();
    assert_eq!(gh.user_codes(), vec![user_code.clone()]);

    let poll_body = format!(
        "client_id=cid&device_code={device_code}&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
    );
    let oauth = gh.oauth_base();
    let poll = || {
        request(
            &oauth,
            "POST",
            "/login/oauth/access_token",
            &form,
            &poll_body,
        )
    };

    let (_, b) = poll().await;
    assert_eq!(json(&b)["error"], "authorization_pending");
    gh.slow_down_next(1);
    let (_, b) = poll().await;
    assert_eq!(json(&b)["error"], "slow_down");
    assert_eq!(json(&b)["interval"], 3);
    assert_eq!(gh.pending_polls(), 2);

    gh.approve_device_code(&user_code, "gho_secret");
    let (_, b) = poll().await;
    assert_eq!(json(&b)["access_token"], "gho_secret");
    // Device codes are single-use.
    let (_, b) = poll().await;
    assert_eq!(json(&b)["error"], "expired_token");

    // Unknown device code and wrong grant type.
    let (_, b) = request(
        &gh.oauth_base(),
        "POST",
        "/login/oauth/access_token",
        &form,
        "client_id=cid&device_code=zzz&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code",
    )
    .await;
    assert_eq!(json(&b)["error"], "incorrect_device_code");
    let (_, b) = request(
        &gh.oauth_base(),
        "POST",
        "/login/oauth/access_token",
        &form,
        "client_id=cid&device_code=zzz",
    )
    .await;
    assert_eq!(json(&b)["error"], "unsupported_grant_type");
}

#[tokio::test]
async fn device_flow_expiry_and_denial_knobs() {
    let gh = FakeGitHub::start().await;
    let hdr = ["Content-Type: application/json"];
    let mut codes = Vec::new();
    for _ in 0..2 {
        let (_, body) = request(
            &gh.oauth_base(),
            "POST",
            "/login/device/code",
            &hdr,
            r#"{"client_id":"cid"}"#,
        )
        .await;
        codes.push(json(&body));
    }
    assert_ne!(codes[0]["device_code"], codes[1]["device_code"]);
    gh.expire_device_code(codes[0]["user_code"].as_str().unwrap());
    gh.deny_device_code(codes[1]["user_code"].as_str().unwrap());
    for (c, want) in codes.iter().zip(["expired_token", "access_denied"]) {
        let body = serde_json::json!({
            "client_id": "cid",
            "device_code": c["device_code"],
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
        })
        .to_string();
        let (_, b) = request(
            &gh.oauth_base(),
            "POST",
            "/login/oauth/access_token",
            &hdr,
            &body,
        )
        .await;
        assert_eq!(json(&b)["error"], want);
    }
    let (_, b) = request(&gh.oauth_base(), "POST", "/login/device/code", &hdr, "{}").await;
    assert_eq!(json(&b)["error"], "unauthorized_client");
}
