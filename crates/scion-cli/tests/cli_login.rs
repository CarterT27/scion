//! SPEC 19.11 through the CLI (P31): device-flow login against the fake GitHub yields
//! `github:<login>` (numeric id in the token); an invalid GitHub or Scion token is an
//! UNAUTHENTICATED exit; the credentials file is written 0600 and a group-readable file is
//! refused; an expired token is re-exchanged silently with the stored GitHub token; service
//! login reads its secret from standard input.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod cli_support;

use cli_support::{Harness, Out, scion, scion_stdin};
use scion_config::MapEnv;
use scion_conformance::TestCluster;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

fn creds_path(h: &Harness) -> PathBuf {
    h.home.path().join(".config/scion/credentials")
}

fn mode_of(p: &std::path::Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// Run `scion [--json] login` and complete the device flow with `gh_token` once the fake
/// GitHub has issued a user code.
async fn device_login(h: &Harness, env: &MapEnv, gh_token: &str, json: bool) -> Out {
    let before = h.cluster.github.user_codes().len();
    let args: Vec<&str> = if json {
        vec!["--json", "login"]
    } else {
        vec!["login"]
    };
    let env2 = env.clone();
    let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
    let run = tokio::spawn(async move {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        scion(&env2, &a).await
    });
    let code = loop {
        let codes = h.cluster.github.user_codes();
        if codes.len() > before {
            break codes[codes.len() - 1].clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    h.cluster.github.approve_device_code(&code, gh_token);
    let out = run.await.unwrap();
    assert!(
        out.stderr.contains(&format!("enter the code: {code}")),
        "the user code must be shown: {}",
        out.stderr
    );
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn device_flow_login_yields_github_principal_with_numeric_id() {
    let h = Harness::start(false).await;
    h.cluster.github.add_user("gho_octo", "octocat", 4242);
    let env = h.env_anon();
    let out = device_login(&h, &env, "gho_octo", true).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    let v = out.json();
    cli_support::validate(&v);
    assert_eq!(v["data"]["principal"], "github:octocat");
    assert_eq!(v["data"]["method"], "github");
    assert_eq!(v["data"]["github_token_stored"], true);

    // Credentials: 0600, holding the Scion token and the GitHub token.
    let path = creds_path(&h);
    assert_eq!(mode_of(&path), 0o600);
    let creds = scion_wc::credentials::read(&path).unwrap();
    assert_eq!(creds.github_token.as_deref(), Some("gho_octo"));
    let claims = h.cluster.servers[0]
        .ctx()
        .verifier
        .verify(&creds.token, SystemTime::now())
        .unwrap();
    assert_eq!(claims.sub.id_string(), "github:octocat");
    assert_eq!(claims.gid, Some(4242));

    // Later commands use the stored token.
    let who = scion(&env, &["whoami"]).await;
    assert_eq!(who.code, 0, "{}", who.stderr);
    assert!(who.text().starts_with("github:octocat\n"), "{}", who.text());
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_github_token_is_unauthenticated_and_writes_nothing() {
    let h = Harness::start(false).await;
    let env = h.env_anon();
    let out = device_login(&h, &env, "gho_not_a_user", false).await;
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("UNAUTHENTICATED"), "{}", out.stderr);
    assert!(!creds_path(&h).exists());

    let out = device_login(&h, &env, "gho_still_not_a_user", true).await;
    assert_eq!(out.code, 1);
    let v = out.json();
    cli_support::validate(&v);
    assert_eq!(v["data"]["code"], "UNAUTHENTICATED");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_scion_token_is_an_unauthenticated_exit() {
    let h = Harness::start(false).await;
    let mut env = h.env_anon();
    env.0.insert("SCION_TOKEN".into(), "v4.public.bogus".into());
    let out = scion(&env, &["whoami"]).await;
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("UNAUTHENTICATED"), "{}", out.stderr);
    let out = scion(&env, &["--json", "log"]).await;
    assert_eq!(out.code, 1);
    assert_eq!(out.json()["data"]["code"], "UNAUTHENTICATED");
}

#[tokio::test(flavor = "multi_thread")]
async fn group_readable_credentials_are_refused() {
    let h = Harness::start(false).await;
    let env = h.env_anon();
    let path = creds_path(&h);
    scion_wc::credentials::write(
        &path,
        &scion_wc::credentials::Credentials {
            token: h.cluster.token_for(&TestCluster::gh("alice")),
            github_token: None,
        },
    )
    .unwrap();
    let ok = scion(&env, &["whoami"]).await;
    assert_eq!(ok.code, 0, "{}", ok.stderr);
    for bad in [0o640, 0o644, 0o604] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(bad)).unwrap();
        let out = scion(&env, &["whoami"]).await;
        assert_eq!(out.code, 1, "mode {bad:o}");
        assert!(out.stderr.contains("PERMISSION_DENIED"), "{}", out.stderr);
        assert!(out.stderr.contains("chmod 600"), "{}", out.stderr);
    }
    // SCION_TOKEN overrides the credentials file entirely (SPEC 6.4).
    let mut with_token = env.clone();
    with_token.0.insert(
        "SCION_TOKEN".into(),
        h.cluster.token_for(&TestCluster::gh("bob")),
    );
    let out = scion(&with_token, &["whoami"]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.text().starts_with("github:bob\n"));
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_token_is_reexchanged_silently() {
    let h = Harness::start(false).await;
    h.cluster.github.add_user("gho_dana", "dana", 77);
    let ctx = h.cluster.servers[0].ctx();
    let who = TestCluster::gh("dana");
    let (expired, _) = ctx
        .keyring
        .issue(
            &who,
            Duration::from_secs(60),
            SystemTime::now() - Duration::from_secs(3600),
        )
        .unwrap();
    assert!(ctx.verifier.verify(&expired, SystemTime::now()).is_err());
    let path = creds_path(&h);
    scion_wc::credentials::write(
        &path,
        &scion_wc::credentials::Credentials {
            token: expired.clone(),
            github_token: Some("gho_dana".into()),
        },
    )
    .unwrap();
    let env = h.env_anon();
    let out = scion(&env, &["--json", "whoami"]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json()["data"]["principal"], "github:dana");
    let fresh = scion_wc::credentials::read(&path).unwrap();
    assert_ne!(fresh.token, expired);
    assert_eq!(fresh.github_token.as_deref(), Some("gho_dana"));
    assert_eq!(mode_of(&path), 0o600);
    let claims = ctx
        .verifier
        .verify(&fresh.token, SystemTime::now())
        .unwrap();
    assert_eq!(claims.sub.id_string(), who.id_string());
    assert_eq!(claims.gid, Some(77));

    // Without a stored GitHub token the expiry surfaces as UNAUTHENTICATED.
    scion_wc::credentials::write(
        &path,
        &scion_wc::credentials::Credentials {
            token: expired,
            github_token: None,
        },
    )
    .unwrap();
    let out = scion(&env, &["whoami"]).await;
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("UNAUTHENTICATED"), "{}", out.stderr);
}

#[tokio::test(flavor = "multi_thread")]
async fn service_login_reads_the_secret_from_stdin() {
    let h = Harness::start(false).await;
    let env = h.env_anon();
    let (name, secret) = TestCluster::SERVICES[1];
    let out = scion_stdin(&env, &["login", "--service", name], &[secret]).await;
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        out.text().contains("Logged in as service:agent"),
        "{}",
        out.text()
    );
    let creds = scion_wc::credentials::read(&creds_path(&h)).unwrap();
    assert_eq!(creds.github_token, None);
    assert_eq!(mode_of(&creds_path(&h)), 0o600);
    let wrong = scion_stdin(&env, &["login", "--service", name], &["wrong"]).await;
    assert_eq!(wrong.code, 1);
    assert!(wrong.stderr.contains("UNAUTHENTICATED"), "{}", wrong.stderr);
    let none = scion(&env, &["login", "--service", name]).await;
    assert_eq!(none.code, 1);
    assert!(none.stderr.contains("no service secret"), "{}", none.stderr);
}
