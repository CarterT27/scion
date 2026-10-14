//! SPEC 19.5 "No locks": the land path (concurrent lands, retries, idempotent retries, update_ref)
//! performs no lease call (`Counters::lease_calls` delta 0) and records no span with target
//! `scion::lock`. A positive control proves the span detector sees lease acquisitions.

mod land_support;

use land_support::*;
use parking_lot::Mutex;
use scion_core::trace::LOCK_TARGET;
use scion_history::{AllowAll, HistoryConfig, LandConfig, LandRequest, UpdateRefRequest};
use scion_store::DerivedKey;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};

/// Records the target of every span created while it is the default dispatcher.
#[derive(Default)]
struct SpanTargets {
    next: AtomicU64,
    targets: Arc<Mutex<Vec<String>>>,
}

impl Subscriber for SpanTargets {
    fn register_callsite(&self, _m: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }
    fn enabled(&self, _m: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, attrs: &Attributes<'_>) -> Id {
        self.targets
            .lock()
            .push(attrs.metadata().target().to_owned());
        Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }
    fn record(&self, _span: &Id, _values: &Record<'_>) {}
    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
    fn event(&self, _event: &Event<'_>) {}
    fn enter(&self, _span: &Id) {}
    fn exit(&self, _span: &Id) {}
}

fn lock_spans(targets: &Mutex<Vec<String>>) -> usize {
    targets.lock().iter().filter(|t| *t == LOCK_TARGET).count()
}

// Current-thread runtime: spawned tasks run on this thread, under the thread-local dispatcher.
#[tokio::test]
async fn land_path_takes_no_lock_or_lease() {
    let sub = SpanTargets::default();
    let targets = sub.targets.clone();
    let _guard = tracing::subscriber::set_default(sub);

    let env = env_with(HistoryConfig {
        land: LandConfig {
            max_attempts: 10_000,
            backoff_base: Duration::from_micros(100),
            backoff_max: Duration::from_millis(5),
            rng_seed: Some(3),
        },
        ..HistoryConfig::default()
    })
    .await;

    // Positive control: a lease call is visible to both detectors.
    let c0 = env.counters.snapshot();
    let key = DerivedKey {
        ty: "control".to_owned(),
        version: 1,
        key: bytes::Bytes::from_static(b"k"),
    };
    assert!(
        env.h
            .meta()
            .try_lease(
                &key,
                "holder",
                Duration::from_secs(1),
                std::time::SystemTime::now()
            )
            .await
            .unwrap()
    );
    assert_eq!((env.counters.snapshot() - c0).lease_calls, 1);
    assert!(lock_spans(&targets) >= 1, "detector sees scion::lock spans");
    targets.lock().clear();

    let n = 20u16;
    let mut heads = Vec::new();
    for i in 0..n {
        let delta = vec![put(&env.h, &format!("n{i}.txt"), &format!("{i}\n")).await];
        heads.push(commit_delta(&env.h, vec![env.root], delta, cid16(i)).await);
    }
    let before = env.counters.snapshot();

    // Concurrent lands (with real CAS contention), forced lost races, and idempotent retries.
    env.contended.force_mismatches(5);
    let mut tasks = Vec::new();
    for head in heads.iter().copied() {
        let h = env.h.clone();
        tasks.push(tokio::spawn(async move {
            let rebaser = FakeRebaser { h: h.clone() };
            h.land_with(
                LandRequest {
                    ref_name: main(),
                    head,
                    allow_empty: false,
                },
                &alice(),
                &AllowAll,
                &rebaser,
            )
            .await
        }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    for head in heads.iter().take(3).copied() {
        env.land(head).await.unwrap();
    }
    // UpdateRef and ref administration.
    let row = env.main_row().await;
    let next = env.commit(row.target, "after.txt", "after\n", 200).await;
    env.h
        .update_ref(
            UpdateRefRequest {
                name: main(),
                expected_version: row.version,
                new_target: next,
                allow_non_fast_forward: false,
            },
            &alice(),
            &Authz::ALL,
        )
        .await
        .unwrap();
    env.h
        .create_ref(&rn("green/ci"), &next, &alice(), "ci")
        .await
        .unwrap();

    let delta = env.counters.snapshot() - before;
    assert_eq!(delta.lease_calls, 0, "no lease on the land path");
    assert_eq!(
        lock_spans(&targets),
        0,
        "no scion::lock span on the land path"
    );
    assert_eq!(env.main_row().await.version, 1 + u64::from(n) + 1);
}
