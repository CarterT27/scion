//! SPEC 19.11 authentication end to end through the daemon (P30), on the Memory backend and,
//! as `external_*`, on CockroachDB + S3 when the gate variables are set:
//!
//! - GitHub login (device flow against the fake GitHub, then `Login{github_access_token}`)
//!   through the daemon: `github:<login>` with the numeric id in the token; invalid GitHub
//!   tokens are UNAUTHENTICATED; with GitHub down new logins are UNAVAILABLE while existing
//!   tokens keep working (SPEC 16.1 class 10).
//! - Service login through the daemon: correct secret succeeds, wrong secret and unknown
//!   names fail, and both paths go through the constant-time primitive.
//! - Expired (including a token that expires during the test), malformed, wrong-key and
//!   tampered tokens are rejected by the server and by the daemon, both on daemon-served and
//!   on forwarded RPCs.
//! - Key rotation: tokens signed by an old key verify at both tiers while the key is listed;
//!   removing it invalidates them.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scion_conformance::{Backend, ClusterOpts, TestCluster, Via, invariants};
use scion_core::{ErrorCode, Principal, Tier};
use scion_daemon::testing::InProcessDaemon;
use scion_proto::{ScionClient, v1};
use scion_server::testing::{InProcessServer, ServerTestOpts};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

fn skip_external() -> bool {
    if Backend::external_available() {
        return false;
    }
    println!("SKIP: SCION_TEST_CRDB_URL / SCION_TEST_S3_ENDPOINT unset");
    true
}

fn gh(login: &str) -> Principal {
    TestCluster::gh(login)
}

/// A daemon-served RPC (ResolveRef) and a forwarded one (WhoAmI) with `client`: both must
/// succeed, or both must fail UNAUTHENTICATED (with the tier's trailers).
async fn probe(client: &ScionClient, via: Via, expect_ok: bool, what: &str) {
    let local = client
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: "main".into(),
            at_version: None,
        })
        .await;
    let fwd = client.auth().who_am_i(v1::WhoAmIRequest {}).await;
    let local_tier = match via {
        Via::Server => Tier::Server,
        Via::Daemon => Tier::Daemon,
    };
    for (rpc, r, tier) in [
        ("ResolveRef", local.map(|_| ()), local_tier),
        ("WhoAmI", fwd.map(|_| ()), Tier::Server),
    ] {
        let label = format!("{what}: {rpc} via {}", via.as_str());
        match (r, expect_ok) {
            (Ok(()), true) => {}
            (Ok(()), false) => panic!("{label}: accepted an invalid token"),
            (Err(s), true) => panic!("{label}: rejected a valid token: {s:?}"),
            (Err(s), false) => {
                assert_eq!(s.code(), tonic::Code::Unauthenticated, "{label}: {s:?}");
                let e = scion_proto::error_from_status(&s);
                assert_eq!(e.code, ErrorCode::Unauthenticated, "{label}");
                assert!(!e.retryable(), "{label}: UNAUTHENTICATED is not retryable");
                let ep = TestCluster::epochs(s.metadata())
                    .unwrap_or_else(|| panic!("{label}: no trailers"));
                assert_eq!(ep.tier, tier, "{label}: tier");
            }
        }
    }
}

fn github_login(token: &str) -> v1::LoginRequest {
    v1::LoginRequest {
        credential: Some(v1::login_request::Credential::GithubAccessToken(
            token.into(),
        )),
    }
}

fn service_login(name: &str, secret: &str) -> v1::LoginRequest {
    v1::LoginRequest {
        credential: Some(v1::login_request::Credential::Service(
            v1::ServiceCredential {
                service_name: name.into(),
                service_secret: secret.into(),
            },
        )),
    }
}

async fn run_github_login(backend: Backend) {
    let c = TestCluster::start(ClusterOpts::new(backend)).await;
    let anon = c.client_with_token(Via::Daemon, None).await;
    // SPEC 12.2 step 1: the device flow parameters come from GetServerInfo.
    let info = anon
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(info.github_oauth_base_url, c.github.oauth_base());
    let dc = scion_auth::device_flow::start(&info.github_oauth_base_url, &info.github_client_id)
        .await
        .unwrap();
    c.github.approve_device_code(&dc.user_code, "gho_e2e_alice");
    c.github.add_user("gho_e2e_alice", "alice", 31337);
    let gh_token =
        scion_auth::device_flow::poll(&info.github_oauth_base_url, &info.github_client_id, &dc)
            .await
            .unwrap();
    assert_eq!(gh_token, "gho_e2e_alice");

    // SPEC 12.2 steps 2-4, through the daemon.
    let r = anon.auth().login(github_login(&gh_token)).await.unwrap();
    let md = r.metadata().clone();
    let r = r.into_inner();
    assert_eq!(TestCluster::epochs(&md).unwrap().tier, Tier::Server);
    assert_eq!(r.principal, "github:alice");
    let claims = c.servers[0]
        .keyring
        .verifier()
        .verify(&r.token, SystemTime::now())
        .unwrap();
    assert_eq!(
        claims.gid,
        Some(31337),
        "the numeric GitHub id is in the token"
    );
    assert_eq!(claims.sub.id_string(), "github:alice");
    assert!(r.expires_at_unix > 0);

    // The issued token works at both tiers.
    for via in Via::ALL {
        let client = c.client_with_token(via, Some(r.token.clone())).await;
        probe(&client, via, true, "GitHub-issued token").await;
        let who = client
            .auth()
            .who_am_i(v1::WhoAmIRequest {})
            .await
            .unwrap()
            .into_inner();
        assert_eq!(who.principal, "github:alice");
    }

    // An access token GitHub does not know.
    let e = anon
        .auth()
        .login(github_login("gho_unknown"))
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::Unauthenticated);
    let d = TestCluster::error_detail(&e).expect("ErrorDetail on a failed login");
    assert_eq!((d.code.as_str(), d.retryable), ("UNAUTHENTICATED", false));
    assert_eq!(
        TestCluster::epochs(e.metadata()).map(|t| t.tier),
        Some(Tier::Server),
        "a forwarded Login error keeps the server's trailers"
    );

    // SPEC 16.1 class 10: GitHub down -> new logins UNAVAILABLE, existing tokens still work.
    c.github.set_down(true);
    let e = anon
        .auth()
        .login(github_login(&gh_token))
        .await
        .unwrap_err();
    let err = scion_proto::error_from_status(&e);
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert!(err.retryable());
    for via in Via::ALL {
        let client = c.client_with_token(via, Some(r.token.clone())).await;
        probe(&client, via, true, "existing token while GitHub is down").await;
    }
    c.github.set_down(false);
    anon.auth().login(github_login(&gh_token)).await.unwrap();
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn github_login_through_the_daemon() {
    run_github_login(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_github_login_through_the_daemon() {
    if skip_external() {
        return;
    }
    run_github_login(Backend::External).await;
}

async fn run_service_login(backend: Backend) {
    let c = TestCluster::start(ClusterOpts::new(backend)).await;
    let anon = c.client_with_token(Via::Daemon, None).await;
    for (name, secret) in TestCluster::SERVICES {
        let before = scion_auth::ct_eq_calls();
        let r = anon
            .auth()
            .login(service_login(name, secret))
            .await
            .unwrap()
            .into_inner();
        assert!(
            scion_auth::ct_eq_calls() > before,
            "a correct secret is compared with the constant-time primitive"
        );
        assert_eq!(r.principal, format!("service:{name}"));
        let claims = c.servers[0]
            .keyring
            .verifier()
            .verify(&r.token, SystemTime::now())
            .unwrap();
        assert_eq!(claims.gid, None);
        for via in Via::ALL {
            let client = c.client_with_token(via, Some(r.token.clone())).await;
            probe(&client, via, true, "service token").await;
        }
        for (what, req) in [
            ("wrong secret", service_login(name, "not-the-secret")),
            (
                "secret prefix",
                service_login(name, &secret[..secret.len() - 1]),
            ),
            ("unknown service", service_login("ghost", secret)),
        ] {
            let before = scion_auth::ct_eq_calls();
            let e = anon.auth().login(req).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::Unauthenticated, "{name}: {what}");
            assert!(
                scion_auth::ct_eq_calls() > before,
                "{name}: {what} is compared with the constant-time primitive"
            );
        }
    }
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn service_login_through_the_daemon() {
    run_service_login(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_service_login_through_the_daemon() {
    if skip_external() {
        return;
    }
    run_service_login(Backend::External).await;
}

/// Flip one character in the middle of the token.
fn tamper(token: &str) -> String {
    let mut b = token.as_bytes().to_vec();
    let i = b.len() / 2;
    b[i] = if b[i] == b'x' { b'y' } else { b'x' };
    String::from_utf8(b).unwrap()
}

async fn run_token_rejection(backend: Backend) {
    let c = TestCluster::start(ClusterOpts::new(backend)).await;
    let alice = gh("alice");
    let keyring = &c.servers[0].keyring;
    let now = SystemTime::now();
    let expired = keyring
        .issue(
            &alice,
            Duration::from_secs(60),
            now - Duration::from_secs(7200),
        )
        .unwrap()
        .0;
    let wrong_key = scion_auth::Keyring::generate_for_tests(1)
        .issue(&alice, Duration::from_secs(3600), now)
        .unwrap()
        .0;
    let valid = c.token_for(&alice);
    let tampered = tamper(&valid);
    let mut forged_sub = keyring
        .issue(&alice, Duration::from_secs(3600), now)
        .unwrap()
        .1;
    forged_sub.sub = gh("admin");
    let forged = scion_auth::Keyring::generate_for_tests(1)
        .sign(&forged_sub)
        .unwrap();
    for via in Via::ALL {
        probe(
            &c.client_with_token(via, Some(valid.clone())).await,
            via,
            true,
            "valid token",
        )
        .await;
        for (what, token) in [
            ("expired", expired.clone()),
            ("malformed", "v4.public.not-a-token".to_owned()),
            ("empty", String::new()),
            ("wrong key", wrong_key.clone()),
            ("tampered", tampered.clone()),
            ("forged claims", forged.clone()),
        ] {
            let client = c.client_with_token(via, Some(token)).await;
            probe(&client, via, false, what).await;
        }
    }
    // A token that expires while in use is rejected afterwards by both tiers.
    // Issued now (not at the test's start: the probes above take time on a loaded machine),
    // with a TTL comfortably longer than the probes below.
    let short_ttl = Duration::from_secs(4);
    let issued = SystemTime::now();
    let short = keyring.issue(&alice, short_ttl, issued).unwrap().0;
    for via in Via::ALL {
        probe(
            &c.client_with_token(via, Some(short.clone())).await,
            via,
            true,
            "short-lived token before expiry",
        )
        .await;
    }
    let expiry = issued + short_ttl + Duration::from_millis(1100);
    if let Ok(wait) = expiry.duration_since(SystemTime::now()) {
        tokio::time::sleep(wait).await;
    }
    for via in Via::ALL {
        probe(
            &c.client_with_token(via, Some(short.clone())).await,
            via,
            false,
            "short-lived token after expiry",
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_tokens_are_rejected_by_server_and_daemon() {
    run_token_rejection(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_invalid_tokens_are_rejected_by_server_and_daemon() {
    if skip_external() {
        return;
    }
    run_token_rejection(Backend::External).await;
}

/// A server over `storage` signing with the first of `keys` and verifying all of them, plus a
/// daemon in front of it.
async fn phase(
    storage: &scion_store::Storage,
    keys: Vec<std::path::PathBuf>,
) -> (InProcessServer, InProcessDaemon) {
    let opts = ServerTestOpts {
        acl_toml: TestCluster::DEFAULT_ACL.to_owned(),
        storage: Some(storage.clone()),
        tweak: Some(Arc::new(move |c| c.auth.signing_keys = keys.clone())),
        ..ServerTestOpts::default()
    };
    let srv = InProcessServer::start(opts).await;
    let d = InProcessDaemon::start(srv.endpoint.clone(), srv.tls.clone(), |_| {}).await;
    (srv, d)
}

async fn clients(
    srv: &InProcessServer,
    d: &InProcessDaemon,
    token: &str,
) -> [(Via, ScionClient); 2] {
    [
        (
            Via::Server,
            ScionClient::connect(&srv.endpoint, srv.tls.clone(), Some(token.to_owned()))
                .await
                .unwrap(),
        ),
        (Via::Daemon, d.client(Some(token.to_owned())).await),
    ]
}

async fn run_rotation(backend: Backend) {
    // The cluster provides the shared storage (and the fake GitHub / log capture).
    let c = TestCluster::start(ClusterOpts::new(backend).with_daemon(false)).await;
    let dir = tempfile::tempdir().unwrap();
    let ring = scion_auth::Keyring::generate_for_tests(2);
    let paths = ring.write_pem_files(dir.path()).unwrap();
    let (old, new) = (paths[0].clone(), paths[1].clone());
    let alice = gh("alice");

    // Phase A: only the old key.
    let (a, da) = phase(&c.storage, vec![old.clone()]).await;
    let old_token = a.token_for(&alice);
    for (via, cl) in clients(&a, &da, &old_token).await {
        probe(&cl, via, true, "old key, before rotation").await;
    }

    // Phase B: the new key signs, the old one still verifies.
    let (b, db) = phase(&c.storage, vec![new.clone(), old.clone()]).await;
    let info = b
        .client(None)
        .await
        .objects()
        .get_server_info(v1::GetServerInfoRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(info.token_public_keys.len(), 2, "both keys are published");
    let new_token = b.token_for(&alice);
    for token in [&old_token, &new_token] {
        for (via, cl) in clients(&b, &db, token).await {
            probe(&cl, via, true, "during rotation").await;
        }
    }
    // The old-key phase does not know the new key.
    for (via, cl) in clients(&a, &da, &new_token).await {
        probe(&cl, via, false, "new key at an old-key-only tier").await;
    }

    // Phase C: the old key is removed: its tokens are invalid at both tiers.
    let (cc, dc) = phase(&c.storage, vec![new.clone()]).await;
    for (via, cl) in clients(&cc, &dc, &old_token).await {
        probe(&cl, via, false, "old key after removal").await;
    }
    for (via, cl) in clients(&cc, &dc, &new_token).await {
        probe(&cl, via, true, "new key after rotation").await;
    }
    invariants::check_16_2(&c).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn key_rotation_at_both_tiers() {
    run_rotation(Backend::Memory).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_key_rotation_at_both_tiers() {
    if skip_external() {
        return;
    }
    run_rotation(Backend::External).await;
}
