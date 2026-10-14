//! SPEC 10.1 / 19.10 resumption: streams killed at random offsets are resumed by re-requesting
//! only incomplete objects, and the working copy is byte-identical; corrupt objects are
//! refused (SPEC 7.3, 16.1 class 7) (P18).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod checkout_support;

use checkout_support::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use scion_core::{ErrorCode, ObjectId};
use scion_testkit::upstream_methods;
use scion_wc::{WcOptions, WorkingCopy};
use std::collections::{BTreeMap, BTreeSet};

fn random_files(rng: &mut StdRng, n: usize) -> Files {
    let mut f = Files::new();
    for i in 0..n {
        let dir = ["", "a/", "a/b/", "c/", "c/d/e/"][i % 5];
        let len = rng.random_range(0..3000usize);
        let mut content = vec![0u8; len];
        rng.fill(&mut content[..]);
        let mode = if i % 7 == 3 { 2 } else { 1 };
        f.insert(
            format!("{dir}file{i:03}").into_bytes(),
            (scion_core::Mode::from_u8(mode).unwrap(), content),
        );
    }
    f.insert(
        b"c/link".to_vec(),
        (scion_core::Mode::Symlink, b"d/e/file004".to_vec()),
    );
    f
}

/// Replay the recorded GetObjects requests against the FakeUpstream kill rule and check that
/// no request asks again for an object an earlier stream already delivered completely.
fn assert_only_incomplete_refetched(
    requests: &[Vec<ObjectId>],
    sizes: &BTreeMap<ObjectId, u64>,
    kill: u64,
) {
    let mut delivered: BTreeMap<ObjectId, u64> = BTreeMap::new();
    let mut complete: BTreeSet<ObjectId> = BTreeSet::new();
    for ids in requests {
        let ranged = ids.len() == 1
            && delivered
                .get(&ids[0])
                .is_some_and(|d| *d > 0 && *d < sizes[&ids[0]]);
        let mut budget = kill;
        for id in ids {
            assert!(
                !complete.contains(id),
                "object {id} was re-requested after it was delivered completely"
            );
            let size = sizes[id];
            let have = if ranged { delivered[id] } else { 0 };
            let remaining = size - have;
            if remaining <= budget {
                budget -= remaining;
                delivered.insert(*id, size);
                complete.insert(*id);
            } else {
                delivered.insert(*id, have + budget);
                break;
            }
        }
    }
    let all: BTreeSet<ObjectId> = sizes.keys().copied().collect();
    assert_eq!(complete, all, "every object delivered");
}

#[tokio::test]
async fn resume_after_random_kills_refetches_only_incomplete_objects() {
    let cases = scion_core::test_support::proptest_cases(6) as u64;
    for seed in 0..cases {
        let mut rng = StdRng::seed_from_u64(0x5c10 + seed);
        let env = Env::start().await;
        let f = random_files(&mut rng, 30);
        env.commit(&f, "main");
        let sizes: BTreeMap<ObjectId, u64> = encode_files(&f)
            .1
            .into_iter()
            .map(|(id, raw)| (id, raw.len() as u64))
            .collect();
        let kill = rng.random_range(150..3000u64);
        env.up.set_chunk_size(rng.random_range(64..512usize));
        env.up.kill_stream_after_bytes(Some(kill));
        let dir = tmp();
        let root = dir.path().join("wc");
        let opts = WcOptions {
            fetch_batch: 4,
            ..WcOptions::default()
        };
        let mut wc = WorkingCopy::checkout(env.client.clone(), &root, "main", Vec::new(), opts)
            .await
            .unwrap_or_else(|e| panic!("seed {seed} kill {kill}: {e}"));
        let snap = scion_wc::testing::snapshot(&root).unwrap();
        assert_eq!(snapshot_files(&snap), f, "seed {seed}: byte-identical");
        let requests: Vec<Vec<ObjectId>> = env
            .up
            .requests_for(upstream_methods::GET_OBJECTS)
            .into_iter()
            .map(|r| r.ids)
            .collect();
        assert!(requests.len() > 1, "the kill knob interrupted streams");
        assert_only_incomplete_refetched(&requests, &sizes, kill);
        env.up.kill_stream_after_bytes(None);
        assert!(wc.status().await.unwrap().is_clean());
    }
}

#[tokio::test]
async fn resume_with_zstd_frames_is_byte_identical() {
    let mut rng = StdRng::seed_from_u64(77);
    let env = Env::start().await;
    let f = random_files(&mut rng, 20);
    env.commit(&f, "main");
    env.up.set_zstd(true);
    env.up.set_chunk_size(100);
    env.up.kill_stream_after_bytes(Some(700));
    let dir = tmp();
    let root = dir.path().join("wc");
    WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .unwrap();
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    assert_eq!(snapshot_files(&snap), f);
}

#[tokio::test]
async fn zstd_without_kills_is_verified_and_written() {
    let env = Env::start().await;
    let f = files(&[("a.txt", 1, "hello\n"), ("b/c.txt", 1, "")]);
    env.commit(&f, "main");
    env.up.set_zstd(true);
    let dir = tmp();
    let root = dir.path().join("wc");
    WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .unwrap();
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    assert_eq!(snapshot_files(&snap), f);
}

#[tokio::test]
async fn corrupt_objects_are_refused_with_integrity() {
    let env = Env::start().await;
    let f = files(&[("a.txt", 1, "good\n"), ("bad.txt", 1, "corrupt me\n")]);
    env.commit(&f, "main");
    env.up.corrupt(ObjectId::hash_blob(b"corrupt me\n"));
    let dir = tmp();
    let root = dir.path().join("wc");
    let err = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .err()
    .expect("corrupt object must fail the checkout");
    assert_eq!(err.code, ErrorCode::Integrity);
    assert!(
        !root.join("bad.txt").exists(),
        "unverified bytes never written"
    );

    // A corrupt tree is refused before any file is written.
    let env = Env::start().await;
    env.commit(&f, "main");
    env.up.corrupt(dir_tree_id(&f, ""));
    let dir = tmp();
    let root = dir.path().join("wc");
    let err = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(err.code, ErrorCode::Integrity);
    assert!(ls(&root).is_empty());
}

#[tokio::test]
async fn corrupt_cached_tree_is_refetched_within_the_same_command() {
    // SPEC 16.1 class 7 at the client tier (amendment W6-3): a cached tree whose bytes fail
    // the digest is discarded and transparently re-fetched by the command that reads it.
    let env = Env::start().await;
    let f = files(&[("a.txt", 1, "one\n"), ("sub/b.txt", 1, "two\n")]);
    env.commit(&f, "main");
    let dir = tmp();
    let root = dir.path().join("wc");
    let wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .unwrap();
    drop(wc);
    let hex = dir_tree_id(&f, "sub").hex();
    let cached = root
        .join(".scion/cache/objects/t")
        .join(&hex[..2])
        .join(&hex);
    let original = std::fs::read(&cached).unwrap();
    let mut bad = original.clone();
    *bad.last_mut().unwrap() ^= 0x01;
    std::fs::write(&cached, &bad).unwrap();

    let mut wc = WorkingCopy::open(&root, env.client.clone(), WcOptions::default()).unwrap();
    let calls = env.client.calls();
    let st = wc.status().await.unwrap();
    assert!(st.is_clean(), "{st:?}");
    assert!(
        env.client.calls() > calls,
        "the corrupt tree was re-fetched"
    );
    assert_eq!(
        std::fs::read(&cached).unwrap(),
        original,
        "a good copy replaced it"
    );
    // Now cached and intact: status needs no server call.
    let calls = env.client.calls();
    assert!(wc.status().await.unwrap().is_clean());
    assert_eq!(env.client.calls(), calls);
}

#[tokio::test]
async fn upstream_down_fails_unavailable_without_writes() {
    let env = Env::start().await;
    let f = files(&[("a.txt", 1, "x\n")]);
    env.commit(&f, "main");
    env.up.set_down(true);
    let dir = tmp();
    let root = dir.path().join("wc");
    let err = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert!(ls(&root).is_empty());
}
