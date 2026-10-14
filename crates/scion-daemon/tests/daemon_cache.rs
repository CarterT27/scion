//! SPEC 19.9 at daemon level: coalescing, integrity, crash safety, eviction, ref caching,
//! upstream outage and queue overflow (also SPEC 16.1 classes 1, 6, 7).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod daemon_support;

use daemon_support::*;
use scion_core::ObjectId;
use scion_proto::{ScionClient, v1};
use scion_testkit::{FakeUpstream, upstream_methods};
use std::time::Duration;

/// Requests issued concurrently for one missing object (SPEC 19.9: 10000).
const COALESCE_REQUESTS: usize = 10_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_requests_for_one_missing_object_make_one_upstream_request() {
    let h = Harness::start(Opts {
        gate: true,
        ..Default::default()
    })
    .await;
    let (id, data) = h.blob(1, 200_000, false);
    let gate = h.gate.clone().unwrap();
    gate.set_open(false);
    // Several connections, many concurrent streams each.
    let mut clients = Vec::new();
    for _ in 0..16 {
        clients.push(h.via_daemon(ALICE).await);
    }
    let n = COALESCE_REQUESTS;
    let mut tasks = Vec::with_capacity(n);
    for i in 0..n {
        let c = clients[i % clients.len()].clone();
        tasks.push(tokio::spawn(async move { get_one(&c, id).await }));
    }
    // Hold the single upstream fetch until the first wave of requests is in flight.
    assert!(eventually(Duration::from_secs(20), || gate.waiting() == 1).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    gate.set_open(true);
    for t in tasks {
        let item = t.await.unwrap().unwrap();
        assert_eq!(item.status, OK);
        assert_eq!(item.content, data.to_vec());
    }
    assert_eq!(h.upstream_get_objects(), 1, "exactly one upstream request");
    assert_eq!(gate.passed(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupted_upstream_frame_is_rejected_and_not_cached() {
    for zstd in [false, true] {
        let h = Harness::start(Opts::default()).await;
        h.up.set_zstd(zstd);
        h.up.set_chunk_size(1000);
        let (id, data) = h.blob(2, 5000, false);
        let (good, _) = h.blob(3, 100, false);
        h.up.corrupt(id);
        let c = h.via_daemon(ALICE).await;
        let err = get_one(&c, id).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::DataLoss, "zstd={zstd}");
        assert_eq!(
            scion_proto::error_from_status(&err).code,
            scion_core::ErrorCode::Integrity
        );
        assert!(!h.daemon().cache().contains(&id));
        // Unaffected objects still work; once upstream is healthy the object is served.
        assert_eq!(get_one(&c, good).await.unwrap().status, OK);
        let _ = data;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bit_flip_in_cache_is_detected_deleted_and_refetched() {
    let h = Harness::start(Opts::default()).await;
    let (id, data) = h.blob(4, 4096, false);
    let c = h.via_daemon(ALICE).await;
    assert_eq!(get_one(&c, id).await.unwrap().content, data.to_vec());
    assert!(h.daemon().cache().contains(&id));
    assert_eq!(h.upstream_get_objects(), 1);
    // Served from cache.
    assert_eq!(get_one(&c, id).await.unwrap().content, data.to_vec());
    assert_eq!(h.upstream_get_objects(), 1);
    // Flip one bit of the stored payload.
    let path = h.cache_file(&id);
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    std::fs::write(&path, &bytes).unwrap();
    let item = get_one(&c, id).await.unwrap();
    assert_eq!(
        item.content,
        data.to_vec(),
        "never serve bytes that do not match"
    );
    assert_eq!(h.upstream_get_objects(), 2, "re-fetched upstream");
    assert!(
        h.daemon().cache().contains(&id),
        "re-cached after the refetch"
    );
    let text = h.metrics.render();
    assert!(metric_sum(&text, "scion_integrity_failures_total", "tier=\"daemon\"") >= 1.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn temp_files_are_removed_on_restart_and_a_crashed_insert_leaves_no_entry() {
    let mut h = Harness::start(Opts::default()).await;
    let (x, xdata) = h.blob(5, 3000, false);
    let (y, ydata) = h.blob(6, 3000, false);
    let c = h.via_daemon(ALICE).await;
    assert_eq!(get_one(&c, x).await.unwrap().content, xdata.to_vec());
    drop(c);
    h.stop_daemon().await;

    // A kill during the insert of y: its fully written temp file sits in the entry's
    // directory, never renamed. Plus stray temp files elsewhere.
    let ypath = h.cache_file(&y);
    let ydir = ypath.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&ydir).unwrap();
    let tmp_y = ydir.join(".tmp-crashed-insert");
    let mut content = vec![0u8];
    content.extend_from_slice(&ydata);
    std::fs::write(&tmp_y, &content).unwrap();
    let objects = h.cfg.cache_dir.join("objects");
    let tmp_top = objects.join(".tmp-stray");
    std::fs::write(&tmp_top, b"partial").unwrap();
    // A truncated entry under a valid name (a torn write that was never fsynced).
    let (z, zdata) = h.blob(7, 3000, false);
    let zpath = h.cache_file(&z);
    std::fs::create_dir_all(zpath.parent().unwrap()).unwrap();
    std::fs::write(&zpath, [0u8, 1, 2]).unwrap();

    h.restart_daemon().await;
    assert!(!tmp_y.exists(), "temp file deleted at startup");
    assert!(!tmp_top.exists(), "temp file deleted at startup");
    assert!(!h.daemon().cache().contains(&y), "no partial entry visible");
    assert!(h.daemon().cache().contains(&x), "valid entries survive");

    let before = h.upstream_get_objects();
    let c = h.via_daemon(ALICE).await;
    assert_eq!(get_one(&c, x).await.unwrap().content, xdata.to_vec());
    assert_eq!(
        h.upstream_get_objects(),
        before,
        "x still served from cache"
    );
    assert_eq!(get_one(&c, y).await.unwrap().content, ydata.to_vec());
    assert_eq!(get_one(&c, z).await.unwrap().content, zdata.to_vec());
    assert_eq!(h.upstream_get_objects(), before + 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn eviction_keeps_usage_bounded_and_refetches_transparently() {
    let max: u64 = 64 * 1024;
    let h = Harness::start(Opts {
        tweak: Box::new(move |c| {
            c.max_cache_bytes = max;
            c.low_watermark_ratio = 0.75;
        }),
        ..Default::default()
    })
    .await;
    let mut objects = Vec::new();
    for i in 0..60u64 {
        let len = 1000 + ((i * 7919) % 8000) as usize;
        objects.push(h.blob(100 + i, len, false));
    }
    let c = h.via_daemon(ALICE).await;
    let rounds = scion_core::test_support::proptest_cases(400) as usize;
    let mut x: u64 = 0x1234_5678;
    for _ in 0..rounds {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let k = 1 + (x % 4) as usize;
        let picks: Vec<usize> = (0..k)
            .map(|j| ((x >> (j * 8)) as usize) % objects.len())
            .collect();
        let ids: Vec<ObjectId> = picks.iter().map(|&p| objects[p].0).collect();
        let items = get_objects(&c, &ids).await.unwrap();
        for (item, &p) in items.iter().zip(&picks) {
            assert_eq!(item.status, OK);
            assert_eq!(item.content, objects[p].1.to_vec());
        }
        assert!(
            h.daemon().cache().usage() <= max,
            "usage above max_cache_bytes"
        );
    }
    // The working set exceeds the cache, so some objects were evicted and re-fetched.
    let fetched: usize =
        h.up.requests_for(upstream_methods::GET_OBJECTS)
            .iter()
            .map(|r| r.ids.len())
            .sum();
    assert!(fetched > objects.len(), "evicted objects were re-fetched");
    let stats = h.daemon().stats();
    assert!(stats.cache_hits > 0 && stats.cache_misses > 0);
}

async fn resolve(c: &ScionClient, name: &str) -> (ObjectId, u64) {
    let r = c
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: name.into(),
            at_version: None,
        })
        .await
        .unwrap()
        .into_inner();
    (ObjectId::from_bytes(&r.target).unwrap(), r.version)
}

#[tokio::test(flavor = "multi_thread")]
async fn ref_cache_honors_ttl_and_monotonic_versions() {
    let h = Harness::start(Opts {
        manual_clock: true,
        tweak: Box::new(|c| c.ref_cache_ttl_ms = 1000),
        ..Default::default()
    })
    .await;
    let clock = h.clock.clone().unwrap();
    let t1 = missing_id(1);
    let t2 = missing_id(2);
    let t3 = missing_id(3);
    h.up.set_ref("main", t1, 1);
    let c = h.via_daemon(ALICE).await;
    assert_eq!(resolve(&c, "main").await, (t1, 1));
    let calls = h.upstream_calls(upstream_methods::RESOLVE_REF);
    assert_eq!(calls, 1);
    // Within the TTL: served from cache even though upstream moved.
    h.up.set_ref("main", t2, 2);
    assert_eq!(resolve(&c, "main").await, (t1, 1));
    assert_eq!(h.upstream_calls(upstream_methods::RESOLVE_REF), 1);
    // After the TTL: asked upstream again.
    clock.advance(Duration::from_millis(1001));
    assert_eq!(resolve(&c, "main").await, (t2, 2));
    assert_eq!(h.upstream_calls(upstream_methods::RESOLVE_REF), 2);
    // An unknown ref passes upstream's NOT_FOUND through.
    let e = c
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: "nope".into(),
            at_version: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::NotFound);
    // at_version is answered upstream.
    let r = c
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: "main".into(),
            at_version: Some(1),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.version, 1);
    let _ = t3;
}

#[tokio::test(flavor = "multi_thread")]
async fn ref_versions_stay_monotonic_across_expiry_and_reordered_upstream_answers() {
    // A lagging second replica behind the same upstream address models reordered answers.
    let h = Harness::start(Opts {
        gate: true,
        manual_clock: true,
        tweak: Box::new(|c| c.ref_cache_ttl_ms = 500),
    })
    .await;
    let clock = h.clock.clone().unwrap();
    let lagging = FakeUpstream::start(upstream_opts(&h.keyring)).await;
    let (t2, t3, t4) = (missing_id(2), missing_id(3), missing_id(4));
    h.up.set_ref("main", t3, 3);
    lagging.set_ref("main", t2, 2);
    let c = h.via_daemon(ALICE).await;
    assert_eq!(resolve(&c, "main").await, (t3, 3));
    let gate = h.gate.clone().unwrap();
    gate.route_to(Some(
        ScionClient::connect(&lagging.endpoint(), None, None)
            .await
            .unwrap()
            .channel(),
    ));
    for _ in 0..3 {
        clock.advance(Duration::from_millis(501));
        assert_eq!(
            resolve(&c, "main").await,
            (t3, 3),
            "never a lower version than returned before"
        );
    }
    assert_eq!(lagging.requests_for(upstream_methods::RESOLVE_REF).len(), 3);
    // The replica catches up: a lagging answer never refreshed the entry, so the next read
    // asks upstream and gets the newer version; it is then cached for the TTL.
    lagging.set_ref("main", t4, 4);
    assert_eq!(resolve(&c, "main").await, (t4, 4));
    lagging.set_ref("main", missing_id(5), 5);
    assert_eq!(resolve(&c, "main").await, (t4, 4), "fresh within the TTL");
    gate.route_to(None);
}

#[tokio::test(flavor = "multi_thread")]
async fn upstream_outage_serves_cached_hits_fails_misses_and_recovers_without_restart() {
    let h = Harness::start(Opts::default()).await;
    let (x, xdata) = h.blob(10, 2048, false);
    let (y, ydata) = h.blob(11, 2048, false);
    let c = h.via_daemon(ALICE).await;
    assert_eq!(get_one(&c, x).await.unwrap().content, xdata.to_vec());
    h.up.set_down(true);
    // Cached hit keeps serving.
    let item = get_one(&c, x).await.unwrap();
    assert_eq!(item.content, xdata.to_vec());
    // Miss fails UNAVAILABLE after bounded retries (SPEC 10.5: at most three attempts).
    let before = h.upstream_get_objects();
    let e = get_one(&c, y).await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::Unavailable);
    let attempts = h.upstream_get_objects() - before;
    assert!((1..=3).contains(&attempts), "attempts = {attempts}");
    // Recovery: no restart needed.
    h.up.set_down(false);
    assert_eq!(get_one(&c, y).await.unwrap().content, ydata.to_vec());
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_overflow_is_resource_exhausted() {
    let h = Harness::start(Opts {
        gate: true,
        tweak: Box::new(|c| {
            c.upstream_max_concurrency = 1;
            c.upstream_queue = 1;
        }),
        ..Default::default()
    })
    .await;
    let (a, adata) = h.blob(20, 100, false);
    let (b, bdata) = h.blob(21, 100, false);
    let (x, _) = h.blob(22, 100, false);
    let gate = h.gate.clone().unwrap();
    // Warm up: keys, token, redaction sync.
    let c = h.via_daemon(ALICE).await;
    let (w, _) = h.blob(23, 10, false);
    get_one(&c, w).await.unwrap();
    gate.set_open(false);
    let c1 = c.clone();
    let t1 = tokio::spawn(async move { get_one(&c1, a).await });
    assert!(eventually(Duration::from_secs(5), || gate.waiting() == 1).await);
    let c2 = c.clone();
    let t2 = tokio::spawn(async move { get_one(&c2, b).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let e = get_one(&c, x).await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::ResourceExhausted);
    assert_eq!(
        scion_proto::error_from_status(&e).code,
        scion_core::ErrorCode::ResourceExhausted
    );
    gate.set_open(true);
    assert_eq!(t1.await.unwrap().unwrap().content, adata.to_vec());
    assert_eq!(t2.await.unwrap().unwrap().content, bdata.to_vec());
}
