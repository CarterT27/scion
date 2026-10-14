//! SPEC 19.5 (library level, fake Rebaser): N concurrent clients landing non-overlapping
//! changes all land, every delta appears exactly once, each change id is landed exactly once,
//! and the land path takes no lease (N = 50 by default; N = 100 and 1000 in the ignored
//! `full_` variant, SCION_FULL).

mod land_support;

use land_support::*;
use scion_history::{AllowAll, HistoryConfig, LandConfig, LandRequest};
use std::collections::BTreeMap;
use std::time::Duration;

async fn concurrent_lands(n: u16) {
    let env = env_with(HistoryConfig {
        land: LandConfig {
            max_attempts: 100_000,
            backoff_base: Duration::from_micros(500),
            backoff_max: Duration::from_millis(u64::from(n).max(10)),
            rng_seed: Some(1),
        },
        ..HistoryConfig::default()
    })
    .await;
    let mut heads = Vec::new();
    for i in 0..n {
        let delta = vec![put(&env.h, &format!("d{i}/f.txt"), &format!("file {i}\n")).await];
        heads.push(commit_delta(&env.h, vec![env.root], delta, cid16(i)).await);
    }
    let before = env.counters.snapshot();
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
        let r = t.await.unwrap().unwrap();
        assert_eq!(r.landed.len(), 1);
    }
    let delta = env.counters.snapshot() - before;
    assert_eq!(delta.lease_calls, 0, "the land path takes no lease");

    let row = env.main_row().await;
    assert_eq!(row.version, 1 + u64::from(n));
    let log = env.log(&main()).await;
    assert_eq!(log.len(), 1 + usize::from(n));
    let mut landed: BTreeMap<_, usize> = BTreeMap::new();
    for (i, e) in log.iter().enumerate() {
        assert_eq!(e.version, i as u64 + 1, "gap-free ref log");
        if i > 0 {
            assert_eq!(e.old_target, log[i - 1].new_target);
            assert_eq!(e.landed.len(), 1);
        }
        for l in &e.landed {
            *landed.entry(l.change_id).or_default() += 1;
        }
    }
    assert_eq!(landed.len(), usize::from(n));
    assert!(
        landed.values().all(|c| *c == 1),
        "each change id landed once"
    );
    for (i, head) in heads.iter().enumerate() {
        let l = env
            .h
            .meta()
            .get_landed(&main(), &cid16(i as u16))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(l.source, *head);
    }
    // The final tree holds every delta exactly once (one file per change, nothing else).
    let root_tree = env
        .h
        .changeset(&row.target)
        .await
        .unwrap()
        .changeset
        .root_tree;
    let changes = env.h.file_diff(None, &root_tree, None).await.unwrap();
    assert_eq!(changes.len(), usize::from(n));
    for i in 0..n {
        assert_eq!(
            env.read(&row.target, &format!("d{i}/f.txt")).await.unwrap(),
            format!("file {i}\n").as_bytes()
        );
    }
    assert_fsck_clean(&env.h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_non_overlapping_lands_all_succeed() {
    concurrent_lands(10).await;
    concurrent_lands(50).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "spec-sized (SCION_FULL): N = 100 and 1000 concurrent lands"]
async fn full_concurrent_non_overlapping_lands_all_succeed() {
    concurrent_lands(100).await;
    concurrent_lands(1000).await;
}
