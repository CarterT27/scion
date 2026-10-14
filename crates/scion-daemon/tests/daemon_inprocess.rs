//! `testing::InProcessDaemon` (SPEC 19.0): start, client, kill, restart over the same cache.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod daemon_support;

use daemon_support::*;
use scion_core::{ObjectId, Principal};
use scion_daemon::testing::InProcessDaemon;
use scion_testkit::FakeUpstream;
use std::time::{Duration, SystemTime};

#[tokio::test(flavor = "multi_thread")]
async fn in_process_daemon_lifecycle() {
    let keyring = scion_auth::Keyring::generate_for_tests(1);
    let up = FakeUpstream::start(upstream_opts(&keyring)).await;
    let data = blob_bytes(1, 5000);
    let id = ObjectId::hash_blob(&data);
    up.insert_object(id, data.clone(), false);
    let token = keyring
        .issue(
            &Principal::parse(ALICE).unwrap(),
            Duration::from_secs(600),
            SystemTime::now(),
        )
        .unwrap()
        .0;
    let mut d = InProcessDaemon::start(up.endpoint(), up.tls(), |c| {
        c.max_cache_bytes = 1 << 20;
    })
    .await;
    assert_eq!(d.cfg.max_cache_bytes, 1 << 20);
    assert!(d.handle.socket_path().starts_with(d.dir.path()));
    let c = d.client(Some(token.clone())).await;
    assert_eq!(get_one(&c, id).await.unwrap().content, data.to_vec());
    drop(c);
    d.kill().await;
    assert!(!d.handle.socket_path().exists());
    d.restart().await;
    assert!(d.handle.cache().contains(&id), "cache survives a restart");
    let before = up
        .requests_for(scion_testkit::upstream_methods::GET_OBJECTS)
        .len();
    let c = d.client(Some(token)).await;
    assert_eq!(get_one(&c, id).await.unwrap().content, data.to_vec());
    assert_eq!(
        up.requests_for(scion_testkit::upstream_methods::GET_OBJECTS)
            .len(),
        before
    );
    d.restart().await;
    d.handle.shutdown().await;
}

/// A stale socket left by a crashed daemon is replaced; a live one is never taken over; a
/// non-socket file at the path is never deleted.
#[tokio::test(flavor = "multi_thread")]
async fn socket_path_takeover_rules() {
    let keyring = scion_auth::Keyring::generate_for_tests(1);
    let up = FakeUpstream::start(upstream_opts(&keyring)).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = scion_config::DaemonConfig::for_tests(dir.path(), &up.endpoint().to_string());
    let start = |cfg: scion_config::DaemonConfig| {
        let conn = std::sync::Arc::new(scion_daemon::DirectUpstream::new(up.endpoint(), up.tls()));
        async move {
            scion_daemon::Daemon::start(
                cfg,
                conn,
                std::sync::Arc::new(scion_core::SystemClock),
                scion_obs::Metrics::new(),
            )
            .await
        }
    };
    // Stale: a bound-then-dropped listener leaves a socket file nobody accepts on.
    drop(std::os::unix::net::UnixListener::bind(&cfg.socket).unwrap());
    assert!(cfg.socket.exists());
    let d = start(cfg.clone()).await.unwrap();
    // Live: a second daemon on the same path is refused and the first keeps serving.
    let e = start(cfg.clone()).await.err().expect("live socket refused");
    assert_eq!(e.code, scion_core::ErrorCode::Unavailable);
    assert!(d.socket_path().exists());
    d.shutdown().await;
    assert!(!cfg.socket.exists());
    // Not a socket: refused, file untouched.
    std::fs::write(&cfg.socket, b"keep").unwrap();
    let e = start(cfg.clone()).await.err().expect("non-socket refused");
    assert_eq!(e.code, scion_core::ErrorCode::InvalidArgument);
    assert_eq!(std::fs::read(&cfg.socket).unwrap(), b"keep");
}
