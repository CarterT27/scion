//! SPEC 19.11 (server side) and 19.16 class 10: GitHub and service login, token rejection on a
//! protected RPC, key rotation through config, GitHub outage (P23).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only helpers outside #[test] fns

mod core_support;

use core_support::{alice, opts};
use scion_auth::{Claims, Keyring};
use scion_core::{Clock, ManualClock, Principal};
use scion_proto::v1;
use scion_server::testing::{InProcessServer, ServerTestOpts};
use scion_testkit::FakeGitHub;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

async fn github_login(
    srv: &InProcessServer,
    token: &str,
) -> Result<v1::LoginResponse, tonic::Status> {
    srv.client(None)
        .await
        .auth()
        .login(v1::LoginRequest {
            credential: Some(v1::login_request::Credential::GithubAccessToken(
                token.to_owned(),
            )),
        })
        .await
        .map(|r| r.into_inner())
}

async fn service_login(
    srv: &InProcessServer,
    name: &str,
    secret: &str,
) -> Result<v1::LoginResponse, tonic::Status> {
    srv.client(None)
        .await
        .auth()
        .login(v1::LoginRequest {
            credential: Some(v1::login_request::Credential::Service(
                v1::ServiceCredential {
                    service_name: name.to_owned(),
                    service_secret: secret.to_owned(),
                },
            )),
        })
        .await
        .map(|r| r.into_inner())
}

async fn who_am_i(srv: &InProcessServer, token: &str) -> Result<v1::WhoAmIResponse, tonic::Status> {
    srv.client(None)
        .await
        .with_token(Some(token.to_owned()))
        .auth()
        .who_am_i(v1::WhoAmIRequest {})
        .await
        .map(|r| r.into_inner())
}

fn verify(srv: &InProcessServer, token: &str) -> Claims {
    srv.ctx()
        .verifier
        .verify(token, srv.ctx().clock.now())
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn github_login_against_fake_github() {
    let gh = FakeGitHub::start().await;
    gh.add_user("gho_valid", "alice", 4242);
    let srv = InProcessServer::start(ServerTestOpts {
        github_api_url: Some(gh.api_url()),
        ..opts()
    })
    .await;

    let resp = github_login(&srv, "gho_valid").await.unwrap();
    assert_eq!(resp.principal, "github:alice");
    let claims = verify(&srv, &resp.token);
    assert_eq!(claims.gid, Some(4242));
    assert_eq!(
        claims.sub,
        Principal::GitHub {
            login: "alice".into(),
            id: Some(4242)
        }
    );
    let exp = claims
        .exp
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(resp.expires_at_unix, exp as i64);
    let ttl = claims.exp.duration_since(claims.iat).unwrap();
    assert_eq!(ttl, srv.config().auth.token_ttl);

    // The issued token works on a protected RPC.
    let me = who_am_i(&srv, &resp.token).await.unwrap();
    assert_eq!(me.principal, "github:alice");
    assert_eq!(me.groups, vec!["eng".to_owned()]);
    assert_eq!(me.expires_at_unix, resp.expires_at_unix);

    // An invalid GitHub token is UNAUTHENTICATED.
    let err = github_login(&srv, "gho_invalid").await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    assert!(!scion_proto::error_from_status(&err).retryable());
    let err = github_login(&srv, "").await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test(flavor = "multi_thread")]
async fn github_down_keeps_existing_tokens_and_fails_new_logins() {
    let gh = FakeGitHub::start().await;
    gh.add_user("gho_bob", "bob", 77);
    let srv = InProcessServer::start(ServerTestOpts {
        github_api_url: Some(gh.api_url()),
        ..opts()
    })
    .await;
    let existing = github_login(&srv, "gho_bob").await.unwrap().token;

    gh.set_down(true);
    let err = github_login(&srv, "gho_bob").await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unavailable);
    let e = scion_proto::error_from_status(&err);
    assert_eq!(e.code, scion_core::ErrorCode::Unavailable);
    assert!(e.retryable());
    // Existing tokens keep working; so do service logins.
    assert_eq!(
        who_am_i(&srv, &existing).await.unwrap().principal,
        "github:bob"
    );
    assert_eq!(
        who_am_i(&srv, &srv.token_for(&alice()))
            .await
            .unwrap()
            .principal,
        "github:alice"
    );

    gh.set_down(false);
    github_login(&srv, "gho_bob").await.unwrap();

    // No GitHub configured at all (unreachable API) behaves the same way.
    let lonely = InProcessServer::start(opts()).await;
    let err = github_login(&lonely, "gho_bob").await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unavailable);
}

#[tokio::test(flavor = "multi_thread")]
async fn service_login_right_and_wrong_secret() {
    let srv = InProcessServer::start(ServerTestOpts {
        service_principals: vec![
            ("ci".into(), "s3cret-ci".into()),
            ("importer".into(), "imp".into()),
        ],
        ..opts()
    })
    .await;
    let ok = service_login(&srv, "ci", "s3cret-ci").await.unwrap();
    assert_eq!(ok.principal, "service:ci");
    let claims = verify(&srv, &ok.token);
    assert_eq!(claims.sub, Principal::Service("ci".into()));
    assert_eq!(claims.gid, None);
    let me = who_am_i(&srv, &ok.token).await.unwrap();
    assert_eq!(me.principal, "service:ci");
    assert_eq!(me.groups, vec!["eng".to_owned()]);

    for (name, secret) in [
        ("ci", "wrong"),
        ("ci", ""),
        ("ci", "imp"),
        ("nobody", "s3cret-ci"),
        ("", ""),
    ] {
        let err = service_login(&srv, name, secret).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated, "{name}/{secret}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_expired_malformed_wrong_key_and_tampered_tokens() {
    let clock = ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    let srv = InProcessServer::start(ServerTestOpts {
        clock: Some(clock.clone()),
        ..opts()
    })
    .await;
    let good = srv.token_for(&alice());
    assert_eq!(
        who_am_i(&srv, &good).await.unwrap().principal,
        "github:alice"
    );

    let other = Keyring::generate_for_tests(1);
    let (wrong_key, _) = other
        .issue(&alice(), Duration::from_secs(3600), clock.now())
        .unwrap();
    let mut tampered = good.clone().into_bytes();
    let i = "v4.public.".len() + 5;
    tampered[i] = if tampered[i] == b'x' { b'y' } else { b'x' };
    let tampered = String::from_utf8(tampered).unwrap();
    let (short, _) = srv
        .keyring
        .issue(&alice(), Duration::from_secs(60), clock.now())
        .unwrap();
    for (what, bad) in [
        ("malformed", "v4.public.not-a-token".to_owned()),
        ("empty-ish", "x".to_owned()),
        ("wrong key", wrong_key),
        ("tampered", tampered),
    ] {
        let err = who_am_i(&srv, &bad).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated, "{what}");
    }
    assert!(who_am_i(&srv, &short).await.is_ok());
    clock.advance(Duration::from_secs(61));
    let err = who_am_i(&srv, &short).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "expired");
    assert!(who_am_i(&srv, &good).await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn key_rotation_through_config() {
    let keys = tempfile::tempdir().unwrap();
    let old = Keyring::generate_for_tests(1);
    let new = Keyring::generate_for_tests(1);
    let old_dir = keys.path().join("old");
    let new_dir = keys.path().join("new");
    std::fs::create_dir_all(&old_dir).unwrap();
    std::fs::create_dir_all(&new_dir).unwrap();
    let old_path = old.write_pem_files(&old_dir).unwrap();
    let new_path = new.write_pem_files(&new_dir).unwrap();
    let (old_token, _) = old
        .issue(&alice(), Duration::from_secs(3600), SystemTime::now())
        .unwrap();

    // Rotated: the new key signs, the old key is still listed and verifies.
    let both: Vec<_> = new_path.iter().chain(old_path.iter()).cloned().collect();
    let srv = InProcessServer::start(ServerTestOpts {
        tweak: Some(Arc::new(move |c| c.auth.signing_keys = both.clone())),
        ..opts()
    })
    .await;
    assert_eq!(
        who_am_i(&srv, &old_token).await.unwrap().principal,
        "github:alice"
    );
    let fresh = srv.token_for(&alice());
    let claims = verify(&srv, &fresh);
    assert_eq!(claims.kid, new.public_keys()[0].kid, "first key signs");
    let info = srv
        .client(None)
        .await
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(info.token_public_keys.len(), 2);
    drop(srv);

    // Removed: the old key no longer verifies.
    let only_new = new_path.clone();
    let srv = InProcessServer::start(ServerTestOpts {
        tweak: Some(Arc::new(move |c| c.auth.signing_keys = only_new.clone())),
        ..opts()
    })
    .await;
    let err = who_am_i(&srv, &old_token).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    assert!(who_am_i(&srv, &srv.token_for(&alice())).await.is_ok());
}
