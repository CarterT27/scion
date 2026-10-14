//! `ServerApp` used directly (as `main.rs` does): build, serve with TLS, graceful shutdown; the
//! binary's startup errors; and the gated CockroachDB + S3 stack (SPEC 3.1, 6, 17; P23).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only helpers outside #[test] fns

mod core_support;

use core_support::{alice, opts};
use scion_auth::{GitHubClient, Keyring};
use scion_config::ServerConfig;
use scion_core::{Clock, SystemClock};
use scion_derived::Registry;
use scion_obs::Metrics;
use scion_policy::{Policy, PolicyHandle};
use scion_proto::{Endpoint, ScionClient, TlsOptions, v1};
use scion_server::testing::{InProcessServer, ServerTestOpts};
use scion_server::{ServerApp, ServerDeps};
use scion_store::{MemOptions, MetaStore, Storage};
use std::sync::Arc;
use std::time::Duration;

/// rcgen CA + server certificate for 127.0.0.1 (amendment O-1).
fn test_tls() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca, ca_key);
    let params = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
    let key = KeyPair::generate().unwrap();
    let cert = params.signed_by(&key, &issuer).unwrap();
    (
        ca_cert.pem().into_bytes(),
        cert.pem().into_bytes(),
        key.serialize_pem().into_bytes(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn build_serve_and_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, _mem) = Storage::in_memory(MemOptions::default());
    let keyring = Keyring::generate_for_tests(1);
    let metrics = Metrics::new();
    let app = ServerApp::build(ServerDeps {
        storage,
        config: ServerConfig::for_tests(dir.path()),
        policy: PolicyHandle::from_policy(Policy::allow_all_for_tests()),
        keyring: keyring.clone(),
        github: GitHubClient::new("http://127.0.0.1:9"),
        clock: Arc::new(SystemClock),
        metrics: metrics.clone(),
        registry: Some(Registry::empty()),
    })
    .await
    .unwrap();
    // Bootstrap ran: main exists at version 1.
    let ctx = app.context();
    let main = ctx
        .history
        .meta()
        .get_ref(&scion_object::RefName::main())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(main.version, 1);
    let _router = app.router();

    let (ca, cert, key) = test_tls();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let handle = app.serve(listener, Some((cert, key))).await.unwrap();
    assert!(handle.admin_addr.is_some());
    let ep = Endpoint::Https(format!("https://127.0.0.1:{}", handle.addr.port()));
    let tls = TlsOptions {
        ca_pem: Some(ca.clone()),
        domain: None,
    };
    let (token, _) = keyring
        .issue(&alice(), Duration::from_secs(60), SystemClock.now())
        .unwrap();
    let c = ScionClient::connect(&ep, Some(tls.clone()), Some(token))
        .await
        .unwrap();
    let me = c.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    assert_eq!(me.into_inner().principal, "github:alice");

    // A client that does not trust the CA cannot connect (TLS is real).
    let other_ca = test_tls().0;
    let bad = ScionClient::connect(
        &ep,
        Some(TlsOptions {
            ca_pem: Some(other_ca),
            domain: None,
        }),
        None,
    )
    .await;
    match bad {
        Err(_) => {}
        Ok(b) => assert!(
            b.objects()
                .get_server_info(v1::GetServerInfoRequest {})
                .await
                .is_err()
        ),
    }

    let addr = handle.addr;
    handle.shutdown().await;
    // The port is free again after shutdown.
    tokio::net::TcpListener::bind(addr).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_rejects_bad_tls_material() {
    let dir = tempfile::tempdir().unwrap();
    let (storage, _mem) = Storage::in_memory(MemOptions::default());
    let app = ServerApp::build(ServerDeps {
        storage,
        config: ServerConfig::for_tests(dir.path()),
        policy: PolicyHandle::from_policy(Policy::allow_all_for_tests()),
        keyring: Keyring::generate_for_tests(1),
        github: GitHubClient::new("http://127.0.0.1:9"),
        clock: Arc::new(SystemClock),
        metrics: Metrics::new(),
        registry: Some(Registry::empty()),
    })
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let err = app
        .serve(listener, Some((b"nope".to_vec(), b"nope".to_vec())))
        .await
        .err()
        .expect("bad TLS material must fail");
    assert_eq!(err.code, scion_core::ErrorCode::InvalidArgument);
}

#[test]
fn binary_fails_on_missing_or_invalid_config() {
    let bin = env!("CARGO_BIN_EXE_scion-server");
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.toml");
    let out = std::process::Command::new(bin)
        .arg("--config")
        .arg(&missing)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("missing.toml"), "{err}");

    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, "[server]\nbogus_key = 1\n").unwrap();
    let out = std::process::Command::new(bin).arg(&bad).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("bogus_key"), "{err}");

    let out = std::process::Command::new(bin)
        .arg("--bogus")
        .output()
        .unwrap();
    assert!(!out.status.success());
}

/// The full stack over CockroachDB and S3 (gated: `SCION_TEST_CRDB_URL`,
/// `SCION_TEST_S3_ENDPOINT`): login-free RPCs, a protected RPC, and data surviving a restart.
#[tokio::test(flavor = "multi_thread")]
async fn external_server_over_crdb_and_s3() {
    let Some(url) = scion_store_crdb::test_url() else {
        println!("SKIP: SCION_TEST_CRDB_URL unset");
        return;
    };
    let Some(s3) = scion_store_s3::test_config() else {
        println!("SKIP: SCION_TEST_S3_ENDPOINT unset");
        return;
    };
    let meta = scion_store_crdb::CrdbMetaStore::connect_fresh_for_tests(&url)
        .await
        .unwrap();
    let blobs = scion_store_s3::S3BlobStore::new(s3).unwrap();
    let storage = Storage::new(
        Arc::new(meta),
        Arc::new(blobs),
        scion_core::FaultRegistry::new(),
    );
    let mut srv = InProcessServer::start(ServerTestOpts {
        storage: Some(storage),
        ..opts()
    })
    .await;
    let c = srv.client(Some(&alice())).await;
    let me = c.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    assert_eq!(me.into_inner().principal, "github:alice");
    let (id, _) = srv
        .ctx()
        .objects
        .put_blob(bytes::Bytes::from_static(b"external bytes"))
        .await
        .unwrap();
    srv.restart().await;
    assert_eq!(
        srv.ctx().objects.get(&id).await.unwrap(),
        &b"external bytes"[..]
    );
    let admin = srv.admin_addr().unwrap();
    assert_eq!(core_support::http_get(admin, "/readyz").await.0, 200);
}
