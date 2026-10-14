//! SPEC 19.9 daemon cache, unit level: request coalescing and the upstream limiter.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use scion_cache::{Coalescer, UpstreamLimiter};
use scion_core::{ErrorCode, ObjectId, ScionError};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// `n` concurrent `get_or_fetch` calls for one key; returns how many times the fetch ran.
async fn coalesce_n(n: usize, fail: bool) -> usize {
    let c: Arc<Coalescer<ObjectId, Arc<Vec<u8>>>> = Arc::new(Coalescer::new());
    let id = ObjectId::hash_blob(b"missing object");
    let fetches = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::with_capacity(n);
    for _ in 0..n {
        let c = Arc::clone(&c);
        let fetches = Arc::clone(&fetches);
        let started = Arc::clone(&started);
        tasks.push(tokio::spawn(async move {
            started.fetch_add(1, Ordering::SeqCst);
            let started2 = Arc::clone(&started);
            c.get_or_fetch(id, move || async move {
                fetches.fetch_add(1, Ordering::SeqCst);
                // Hold the upstream request open until every caller has joined.
                while started2.load(Ordering::SeqCst) < n {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                if fail {
                    Err(ScionError::unavailable(
                        "upstream_unavailable",
                        "upstream down",
                    ))
                } else {
                    Ok(Arc::new(b"payload".to_vec()))
                }
            })
            .await
        }));
    }
    for t in tasks {
        let r = t.await.unwrap();
        if fail {
            let e = r.unwrap_err();
            assert_eq!(e.code, ErrorCode::Unavailable);
            assert_eq!(e.subcode(), Some("upstream_unavailable"));
        } else {
            assert_eq!(&r.unwrap()[..], b"payload");
        }
    }
    assert_eq!(c.inflight(), 0, "entry removed after completion");
    fetches.load(Ordering::SeqCst)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ten_thousand_concurrent_requests_fetch_once() {
    assert_eq!(coalesce_n(10_000, false).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn errors_are_shared_by_the_group() {
    assert_eq!(coalesce_n(1_000, true).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn distinct_keys_fetch_independently() {
    let c: Arc<Coalescer<u32, u32>> = Arc::new(Coalescer::new());
    let fetches = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for i in 0..400u32 {
        let c = Arc::clone(&c);
        let fetches = Arc::clone(&fetches);
        tasks.push(tokio::spawn(async move {
            let key = i % 4;
            c.get_or_fetch(key, move || async move {
                fetches.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(key * 10)
            })
            .await
            .map(|v| (key, v))
        }));
    }
    for t in tasks {
        let (k, v) = t.await.unwrap().unwrap();
        assert_eq!(v, k * 10);
    }
    // Each key ran at least once and far fewer times than its callers.
    let n = fetches.load(Ordering::SeqCst);
    assert!((4..=40).contains(&n), "fetches = {n}");
    assert_eq!(c.inflight(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_overflow_is_resource_exhausted() {
    let l = Arc::new(UpstreamLimiter::new(2, 3));
    let p1 = l.acquire().await.unwrap();
    let p2 = l.acquire().await.unwrap();
    assert_eq!(l.available(), 0);
    // Three callers may queue.
    let mut waiters = Vec::new();
    for _ in 0..3 {
        let l = Arc::clone(&l);
        waiters.push(tokio::spawn(async move { l.acquire().await.map(|_p| ()) }));
    }
    while l.waiting() < 3 {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // The fourth overflows immediately.
    let err = tokio::time::timeout(Duration::from_secs(5), l.acquire())
        .await
        .expect("overflow must not block")
        .err()
        .expect("overflow must fail");
    assert_eq!(err.code, ErrorCode::ResourceExhausted);
    assert!(err.retryable());
    // Releasing slots drains the queue.
    drop(p1);
    drop(p2);
    for w in waiters {
        w.await.unwrap().unwrap();
    }
    assert_eq!(l.waiting(), 0);
    assert_eq!(l.available(), 2);
    let _p = l.acquire().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrency_never_exceeds_the_cap() {
    let max = 4;
    let l = Arc::new(UpstreamLimiter::new(max, 10_000));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for _ in 0..200 {
        let (l, active, peak) = (Arc::clone(&l), Arc::clone(&active), Arc::clone(&peak));
        tasks.push(tokio::spawn(async move {
            let _p = l.acquire().await.unwrap();
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(2)).await;
            active.fetch_sub(1, Ordering::SeqCst);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let p = peak.load(Ordering::SeqCst);
    assert!(p <= max && p >= 1, "peak = {p}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn zero_queue_rejects_when_busy() {
    let l = UpstreamLimiter::new(1, 0);
    let p = l.acquire().await.unwrap();
    assert_eq!(
        l.acquire().await.err().unwrap().code,
        ErrorCode::ResourceExhausted
    );
    drop(p);
    let _p = l.acquire().await.unwrap();
}
