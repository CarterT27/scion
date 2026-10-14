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
