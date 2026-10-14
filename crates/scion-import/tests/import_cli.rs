//! The `scion-import` binary (SPEC 14 CLI; implementation-defined 6.4): service login with
//! `SCION_SERVICE_NAME` / `SCION_SERVICE_SECRET` over HTTPS with `--ca-file`, `SCION_TOKEN`
//! over a Unix socket, `--skip-submodules`, JSON report on stdout, failures on stderr with a
//! non-zero exit status (P29).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod import_support;

use import_support::*;
use scion_import::testing::{IMPORTER_SECRET, IMPORTER_SERVICE, importer_principal};
use scion_proto::Endpoint;
use std::path::Path;
use std::process::Output;

fn endpoint_text(ep: &Endpoint) -> String {
    match ep {
        Endpoint::Unix(p) => format!("unix://{}", p.display()),
        Endpoint::Https(u) => u.clone(),
    }
}

async fn run(args: Vec<String>, envs: Vec<(&'static str, String)>) -> Output {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_scion-import"));
    cmd.args(args)
        .env_remove("SCION_TOKEN")
        .env_remove("SCION_SERVICE_NAME")
        .env_remove("SCION_SERVICE_SECRET")
        .env_remove("SCION_ENDPOINT")
        .env_remove("SCION_CA_FILE");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().await.expect("run scion-import")
}

fn args(repo: &Path, git_ref: &str, target: &str, endpoint: &str, extra: &[&str]) -> Vec<String> {
    let mut v = vec![
        "--git".to_owned(),
        repo.display().to_string(),
        "--ref".to_owned(),
        git_ref.to_owned(),
        "--target-ref".to_owned(),
        target.to_owned(),
        "--endpoint".to_owned(),
        endpoint.to_owned(),
    ];
    v.extend(extra.iter().map(|s| (*s).to_owned()));
    v
}

fn report(out: &Output) -> serde_json::Value {
    assert!(
        out.status.success(),
        "scion-import failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("JSON report on stdout")
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_service_login_over_https() {
    let s = server().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = unpack("synthetic-edge", tmp.path());
    let ca = tmp.path().join("ca.pem");
    std::fs::write(&ca, s.tls.as_ref().unwrap().ca_pem.as_ref().unwrap()).unwrap();
    let ep = endpoint_text(&s.endpoint);
    let service = vec![
        ("SCION_SERVICE_NAME", IMPORTER_SERVICE.to_owned()),
        ("SCION_SERVICE_SECRET", IMPORTER_SECRET.to_owned()),
    ];
    let ca_s = ca.display().to_string();

    // A gitlink without --skip-submodules fails with a message on stderr.
    let out = run(
        args(&repo, "main", "imp/cli", &ep, &["--ca-file", &ca_s]),
        service.clone(),
    )
    .await;
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--skip-submodules"), "{err}");

    let out = run(
        args(
            &repo,
            "main",
            "imp/cli",
            &ep,
            &[
                "--ca-file",
                &ca_s,
                "--skip-submodules",
                "--batch-bytes",
                "64",
            ],
        ),
        service.clone(),
    )
    .await;
    let r = report(&out);
    assert!(r["imported"].as_u64().unwrap() > 0);
    assert_eq!(r["ref_version"], 1);
    assert_eq!(r["skipped_gitlinks"][0][0], "vendor/sub");
    let c = importer_client(&s).await;
    let (target, _) = resolve_ref(&c, "imp/cli").await.unwrap();
    assert_eq!(r["final_target"].as_str().unwrap(), target.to_text());

    // The endpoint and CA may come from the environment; a re-run imports nothing.
    let mut env = service.clone();
    env.push(("SCION_ENDPOINT", ep.clone()));
    env.push(("SCION_CA_FILE", ca_s.clone()));
    let a: Vec<String> = [
        "--git",
        &repo.display().to_string(),
        "--ref",
        "main",
        "--target-ref",
        "imp/cli",
        "--skip-submodules",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let r = report(&run(a, env).await);
    assert_eq!(r["imported"], 0);
    assert_eq!(r["skipped_existing"], r["commits_seen"]);

    // A wrong secret fails the login.
    let out = run(
        args(&repo, "main", "imp/cli", &ep, &["--ca-file", &ca_s]),
        vec![
            ("SCION_SERVICE_NAME", IMPORTER_SERVICE.to_owned()),
            ("SCION_SERVICE_SECRET", "wrong".to_owned()),
        ],
    )
    .await;
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("login"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_token_over_unix_socket() {
    let s = server_with(false).await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = unpack("cfg-if", tmp.path());
    let ep = endpoint_text(&s.endpoint);
    assert!(ep.starts_with("unix://"));
    let token = s.token_for(&importer_principal());
    let r = report(
        &run(
            args(&repo, "HEAD", "imp/tok", &ep, &[]),
            vec![("SCION_TOKEN", token)],
        )
        .await,
    );
    assert!(r["imported"].as_u64().unwrap() > 100);
    assert_eq!(r["imported"], r["commits_seen"]);

    // No credentials at all.
    let out = run(args(&repo, "HEAD", "imp/tok", &ep, &[]), vec![]).await;
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("SCION_TOKEN"), "{err}");

    // An invalid target ref name.
    let out = run(
        args(&repo, "HEAD", "Bad Ref", &ep, &[]),
        vec![("SCION_TOKEN", s.token_for(&importer_principal()))],
    )
    .await;
    assert!(!out.status.success());
}
