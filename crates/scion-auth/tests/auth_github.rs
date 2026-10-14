//! GitHubClient and the device flow against FakeGitHub (SPEC 12.2, 16.1 class 10, 19.11).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only helpers outside #[test] fns

use scion_auth::GitHubClient;
use scion_auth::device_flow::{self, DeviceCode};
use scion_core::{ErrorCode, subcode};
use scion_testkit::FakeGitHub;
use std::time::Duration;

#[tokio::test]
async fn valid_token_maps_login_and_id() {
    let gh = FakeGitHub::start().await;
    gh.add_user("gho_alice", "alice", 4242);
    gh.add_user("gho_bob", "bob", 7);
    let c = GitHubClient::new(&gh.api_url());
    let u = c.user("gho_alice").await.unwrap();
    assert_eq!((u.login.as_str(), u.id), ("alice", 4242));
    // Trailing slash in the configured URL is tolerated.
    let c2 = GitHubClient::new(&format!("{}/", gh.api_url()));
    assert_eq!(c2.user("gho_bob").await.unwrap().id, 7);
}

#[tokio::test]
async fn invalid_token_is_unauthenticated() {
    let gh = FakeGitHub::start().await;
    gh.add_user("gho_alice", "alice", 4242);
    let c = GitHubClient::new(&gh.api_url());
    for bad in ["gho_mallory", "", "gho_alice2"] {
        let e = c.user(bad).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Unauthenticated, "{bad:?}: {e}");
        assert!(!e.message.contains("gho_"), "token leaked into error: {e}");
    }
}

#[tokio::test]
async fn github_down_is_unavailable() {
    let gh = FakeGitHub::start().await;
    gh.add_user("gho_alice", "alice", 4242);
    let c = GitHubClient::new(&gh.api_url());
    gh.set_down(true);
    let e = c.user("gho_alice").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unavailable, "{e}");
    assert_eq!(e.subcode(), Some(subcode::UPSTREAM_UNAVAILABLE));
    assert!(e.retryable());
    gh.set_down(false);
    assert_eq!(c.user("gho_alice").await.unwrap().login, "alice");
}

#[tokio::test]
async fn transport_error_is_unavailable() {
    // Bind and drop a listener so the port is (almost certainly) closed.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let url = format!("http://127.0.0.1:{port}");
    let e = GitHubClient::new(&url).user("x").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unavailable, "{e}");
    let e = device_flow::start(&url, "cid").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unavailable, "{e}");
}

#[tokio::test]
async fn non_json_user_response_is_unavailable() {
    // An endpoint that answers 200 with something that is not a GitHub user.
    let gh = FakeGitHub::start().await;
    gh.set_down(true);
    let srv = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/user"))
        .and(wiremock::matchers::header("authorization", "Bearer tok"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("<html>"))
        .mount(&srv)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/user"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer forbidden",
        ))
        .respond_with(wiremock::ResponseTemplate::new(403))
        .mount(&srv)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/user"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer bad-gateway",
        ))
        .respond_with(wiremock::ResponseTemplate::new(502))
        .mount(&srv)
        .await;
    let c = GitHubClient::new(&srv.uri());
    assert_eq!(
        c.user("tok").await.unwrap_err().code,
        ErrorCode::Unavailable
    );
    assert_eq!(
        c.user("forbidden").await.unwrap_err().code,
        ErrorCode::Unauthenticated
    );
    assert_eq!(
        c.user("bad-gateway").await.unwrap_err().code,
        ErrorCode::Unavailable
    );
}

async fn wait_for_pending(gh: &FakeGitHub, at_least: u64) {
    for _ in 0..500 {
        if gh.pending_polls() >= at_least {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("poll never reported authorization_pending");
}

#[tokio::test]
async fn device_flow_round_trip_with_pending() {
    let gh = FakeGitHub::start().await;
    gh.add_user("gho_device", "carol", 99);
    let dc = device_flow::start(&gh.oauth_base(), "client-123")
        .await
        .unwrap();
    assert!(!dc.device_code.is_empty());
    assert!(!dc.user_code.is_empty());
    assert!(dc.verification_uri.starts_with("https://"));
    assert_eq!(dc.expires_in, Duration::from_secs(900));
    assert_eq!(gh.user_codes(), vec![dc.user_code.clone()]);

    let base = gh.oauth_base();
    let dc2 = dc.clone();
    let poller = tokio::spawn(async move { device_flow::poll(&base, "client-123", &dc2).await });
    // The flow must see authorization_pending at least twice before approval.
    wait_for_pending(&gh, 2).await;
    assert!(!poller.is_finished());
    gh.approve_device_code(&dc.user_code, "gho_device");
    let token = tokio::time::timeout(Duration::from_secs(10), poller)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(token, "gho_device");

    // The obtained token logs in.
    let u = GitHubClient::new(&gh.api_url()).user(&token).await.unwrap();
    assert_eq!((u.login.as_str(), u.id), ("carol", 99));
}

#[tokio::test]
async fn device_flow_slow_down_then_success() {
    let gh = FakeGitHub::start().await;
    let dc = device_flow::start(&gh.oauth_base(), "cid").await.unwrap();
    gh.slow_down_next(2);
    let base = gh.oauth_base();
    let dc2 = dc.clone();
    let poller = tokio::spawn(async move { device_flow::poll(&base, "cid", &dc2).await });
    wait_for_pending(&gh, 3).await; // two slow_down + one authorization_pending
    gh.approve_device_code(&dc.user_code, "gho_slow");
    let token = tokio::time::timeout(Duration::from_secs(10), poller)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(token, "gho_slow");
}

#[tokio::test]
async fn device_flow_expired_and_denied() {
    let gh = FakeGitHub::start().await;
    let a = device_flow::start(&gh.oauth_base(), "cid").await.unwrap();
    gh.expire_device_code(&a.user_code);
    let e = device_flow::poll(&gh.oauth_base(), "cid", &a)
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Unauthenticated, "{e}");

    let b = device_flow::start(&gh.oauth_base(), "cid").await.unwrap();
    gh.deny_device_code(&b.user_code);
    let e = device_flow::poll(&gh.oauth_base(), "cid", &b)
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Unauthenticated, "{e}");

    // Client-side expiry: never approved, expires_in elapses.
    let c = device_flow::start(&gh.oauth_base(), "cid").await.unwrap();
    let short = DeviceCode {
        expires_in: Duration::from_millis(150),
        ..c
    };
    let e = tokio::time::timeout(
        Duration::from_secs(10),
        device_flow::poll(&gh.oauth_base(), "cid", &short),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Unauthenticated, "{e}");

    // Unknown device code.
    let bogus = DeviceCode {
        device_code: "nope".into(),
        user_code: "NOPE".into(),
        verification_uri: "https://github.com/login/device".into(),
        interval: Duration::ZERO,
        expires_in: Duration::from_secs(5),
    };
    let e = device_flow::poll(&gh.oauth_base(), "cid", &bogus)
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Unauthenticated, "{e}");
}

#[tokio::test]
async fn device_flow_github_down_is_unavailable() {
    let gh = FakeGitHub::start().await;
    let dc = device_flow::start(&gh.oauth_base(), "cid").await.unwrap();
    gh.set_down(true);
    let e = device_flow::start(&gh.oauth_base(), "cid")
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Unavailable, "{e}");
    let e = device_flow::poll(&gh.oauth_base(), "cid", &dc)
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Unavailable, "{e}");
    // An empty client id is refused by the (up) fake.
    gh.set_down(false);
    let e = device_flow::start(&gh.oauth_base(), "").await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unauthenticated, "{e}");
}

#[tokio::test]
async fn github_down_does_not_affect_existing_tokens() {
    // SPEC 16.1 class 10: existing Scion tokens keep verifying while GitHub is down; only new
    // human logins fail.
    let gh = FakeGitHub::start().await;
    gh.add_user("gho_alice", "alice", 4242);
    let k = scion_auth::Keyring::generate_for_tests(1);
    let now = std::time::SystemTime::now();
    let u = GitHubClient::new(&gh.api_url())
        .user("gho_alice")
        .await
        .unwrap();
    let who = scion_core::Principal::GitHub {
        login: u.login,
        id: Some(u.id),
    };
    let (tok, _) = k.issue(&who, Duration::from_secs(3600), now).unwrap();
    gh.set_down(true);
    assert_eq!(k.verifier().verify(&tok, now).unwrap().sub, who);
    assert_eq!(
        GitHubClient::new(&gh.api_url())
            .user("gho_alice")
            .await
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
}
