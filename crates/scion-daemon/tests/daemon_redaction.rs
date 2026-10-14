//! SPEC 19.13 at the daemon: redactions are learned within one poll interval plus one request
//! (and immediately from a higher trailer epoch), cached copies are deleted, unredactions
//! propagate, and a stale redaction set makes the daemon fail closed (SPEC 13.3, 18.8).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod daemon_support;

use daemon_support::*;
use scion_core::Tier;
use scion_proto::v1;
use scion_testkit::upstream_methods;
use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread")]
async fn redaction_is_learned_within_one_poll_interval_and_cached_copy_deleted() {
    let interval = Duration::from_millis(200);
    let h = Harness::start(Opts {
        tweak: Box::new(move |c| {
            c.redaction_poll_interval_ms = interval.as_millis() as u64;
            c.redaction_max_staleness_ms = 60_000;
        }),
        ..Default::default()
    })
    .await;
    let (x, xdata) = h.blob(1, 3000, false);
    let a = h.via_daemon(ALICE).await;
    assert_eq!(get_one(&a, x).await.unwrap().content, xdata.to_vec());
    assert!(h.daemon().cache().contains(&x));
    let redacted_at = Instant::now();
    h.up.redact(x);
    // One poll interval later (plus scheduling slack), one request sees `redacted`.
    tokio::time::sleep(interval + Duration::from_millis(100)).await;
    let item = get_one(&a, x).await.unwrap();
    assert_eq!(item.status, REDACTED);
    assert!(item.content.is_empty());
    assert!(!h.daemon().cache().contains(&x), "cached copy deleted");
    assert!(!h.cache_file(&x).exists());
    assert_eq!(h.daemon().stats().redaction_epoch, 1);
    assert!(redacted_at.elapsed() < interval * 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn higher_trailer_epoch_triggers_an_immediate_poll() {
    let h = Harness::start(Opts {
        tweak: Box::new(|c| {
            c.redaction_poll_interval_ms = 600_000;
            c.redaction_max_staleness_ms = 600_000;
        }),
        ..Default::default()
    })
    .await;
    let (x, _) = h.blob(1, 3000, false);
    let a = h.via_daemon(ALICE).await;
    get_one(&a, x).await.unwrap();
    h.up.redact(x);
    // Any upstream response carries the new epoch in its trailers.
    a.auth().who_am_i(v1::WhoAmIRequest {}).await.unwrap();
    assert!(
        eventually(Duration::from_secs(5), || !h.daemon().cache().contains(&x)).await,
        "cached copy deleted after the trailer-triggered poll"
    );
    assert_eq!(get_one(&a, x).await.unwrap().status, REDACTED);
    assert!(h.upstream_calls(upstream_methods::LIST_REDACTIONS) >= 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn unaccounted_epochs_relist_from_zero_so_unredactions_propagate() {
    let h = Harness::start(Opts {
        tweak: Box::new(|c| c.redaction_poll_interval_ms = 50),
        ..Default::default()
    })
    .await;
    let (x, xdata) = h.blob(1, 100, false);
    let (y, _) = h.blob(2, 100, false);
    h.up.redact(y);
    let a = h.via_daemon(ALICE).await;
    assert_eq!(get_one(&a, y).await.unwrap().status, REDACTED);
    assert!(
        eventually(Duration::from_secs(5), || h
            .daemon()
            .stats()
            .redaction_epoch
            == 1)
        .await
    );
    // The upstream epoch moves without rows for the gap (an unredaction): the daemon re-lists
    // from 0 and replaces its set.
    h.up.set_epochs(0, 5);
    assert!(
        eventually(Duration::from_secs(5), || h
            .daemon()
            .stats()
            .redaction_epoch
            == 5)
        .await
    );
    let since: Vec<_> =
        h.up.requests_for(upstream_methods::LIST_REDACTIONS)
            .into_iter()
            .collect();
    assert!(since.len() >= 3);
    assert_eq!(get_one(&a, x).await.unwrap().content, xdata.to_vec());
    assert_eq!(
        get_one(&a, y).await.unwrap().status,
        REDACTED,
        "still redacted upstream"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_redaction_set_fails_closed_and_recovers_after_a_refresh() {
    let h = Harness::start(Opts {
        manual_clock: true,
        tweak: Box::new(|c| {
            c.redaction_poll_interval_ms = 50;
            c.redaction_max_staleness_ms = 1000;
        }),
        ..Default::default()
    })
    .await;
    let clock = h.clock.clone().unwrap();
    let (x, xdata) = h.blob(1, 3000, false);
    let a = h.via_daemon(ALICE).await;
    assert_eq!(get_one(&a, x).await.unwrap().content, xdata.to_vec());
    let before = h.upstream_get_objects();
    get_one(&a, x).await.unwrap();
    assert_eq!(
        h.upstream_get_objects(),
        before,
        "normally served from cache"
    );
    assert!(!h.daemon().stats().fail_closed);

    h.daemon().block_redaction_polling(true);
    clock.advance(Duration::from_millis(1500));
    assert!(h.daemon().stats().fail_closed);
    // Fail closed: the cached object is not served from cache; the read is forwarded.
    let item = get_one(&a, x).await.unwrap();
    assert_eq!(item.content, xdata.to_vec());
    assert_eq!(h.upstream_get_objects(), before + 1, "forwarded upstream");
    assert_eq!(a.last_epochs().unwrap().tier, Tier::Server);
    // A redaction upstream is honored at once because reads are forwarded.
    h.up.redact(x);
    assert_eq!(get_one(&a, x).await.unwrap().status, REDACTED);

    // Unblock: the next successful refresh resumes normal mode and learns the redaction.
    h.daemon().block_redaction_polling(false);
    assert!(eventually(Duration::from_secs(5), || !h.daemon().stats().fail_closed).await);
    assert!(eventually(Duration::from_secs(5), || !h.daemon().cache().contains(&x)).await);
    let (y, ydata) = h.blob(2, 100, false);
    assert_eq!(get_one(&a, y).await.unwrap().content, ydata.to_vec());
    let n = h.upstream_get_objects();
    assert_eq!(get_one(&a, y).await.unwrap().content, ydata.to_vec());
    assert_eq!(h.upstream_get_objects(), n, "cache serving resumed");
    assert_eq!(a.last_epochs().unwrap().tier, Tier::Daemon);
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_resyncs_redactions_before_serving_cached_objects() {
    let mut h = Harness::start(Opts {
        tweak: Box::new(|c| c.redaction_poll_interval_ms = 600_000),
        ..Default::default()
    })
    .await;
    let (x, _) = h.blob(1, 100, false);
    let a = h.via_daemon(ALICE).await;
    get_one(&a, x).await.unwrap();
    drop(a);
    h.stop_daemon().await;
    // Redacted while the daemon was down.
    h.up.redact(x);
    h.restart_daemon().await;
    assert!(h.daemon().cache().contains(&x));
    let a = h.via_daemon(ALICE).await;
    assert_eq!(get_one(&a, x).await.unwrap().status, REDACTED);
    assert!(!h.daemon().cache().contains(&x));
}
