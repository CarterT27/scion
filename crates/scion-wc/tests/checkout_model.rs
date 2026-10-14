//! SPEC 19.10: the checkout result equals the reference model's materialization (contents,
//! executable bits, symlinks) for random trees and random sparse profiles (P18).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod checkout_support;

use checkout_support::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use scion_object::RepoPath;
use scion_testkit::{CorpusSpec, generate};
use scion_wc::{WcOptions, WorkingCopy};

/// A random sparse profile: whole tree, or 1-3 existing directories/files.
fn random_profile(rng: &mut StdRng, f: &Files) -> Vec<Vec<u8>> {
    if rng.random_bool(0.25) || f.is_empty() {
        return vec![Vec::new()];
    }
    let paths: Vec<&Vec<u8>> = f.keys().collect();
    let mut out = Vec::new();
    for _ in 0..rng.random_range(1..=3) {
        let p = paths[rng.random_range(0..paths.len())];
        let comps: Vec<&[u8]> = p.split(|b| *b == b'/').collect();
        let keep = rng.random_range(1..=comps.len());
        out.push(comps[..keep].join(&b'/'));
    }
    out.sort();
    out.dedup();
    out
}

async fn run_cases(cases: u64, files_n: u64) {
    let env = Env::start().await;
    let s = env.scenario();
    for seed in 0..cases {
        let repo = generate(&CorpusSpec {
            files: files_n,
            depth: 4,
            fanout: 3,
            history_len: 3,
            seed: 1000 + seed,
            ..CorpusSpec::default()
        });
        let last = repo.commits.last().unwrap();
        let f = from_triples(
            last.tree
                .iter()
                .map(|(p, m)| (p.clone(), m.mode, m.content.clone())),
        );
        let refname = format!("case{seed}");
        env.commit(&f, &refname);
        let mut rng = StdRng::seed_from_u64(seed);
        let profile = random_profile(&mut rng, &f);
        let sparse: Vec<RepoPath> = profile
            .iter()
            .map(|p| RepoPath::parse(p).unwrap())
            .collect();
        let snap = scion_wc::testing::checkout_snapshot(&s, &refname, &sparse)
            .await
            .unwrap();
        assert_eq!(
            snapshot_files(&snap),
            materialize(&f, &profile),
            "seed {seed}, profile {profile:?}"
        );
    }
}

#[tokio::test]
async fn checkout_matches_model_materialization_for_random_trees() {
    run_cases(scion_core::test_support::proptest_cases(12) as u64, 60).await;
}

#[tokio::test]
#[ignore = "full_: larger randomized run (SCION_FULL)"]
async fn full_checkout_matches_model_materialization() {
    run_cases(200, 300).await;
}

#[tokio::test]
async fn edge_case_repositories_check_out_like_the_model() {
    let env = Env::start().await;
    let s = env.scenario();
    for (name, repo) in scion_testkit::edge_cases() {
        let Some(last) = repo.commits.last() else {
            continue;
        };
        let f = from_triples(
            last.tree
                .iter()
                .map(|(p, m)| (p.clone(), m.mode, m.content.clone())),
        );
        // Entries the client must refuse (SPEC 4.4) or cannot represent are covered by the
        // safety tests; here only representable trees are compared.
        if f.iter().any(|(p, (m, c))| {
            scion_wc::fs_safe::validate_entry_path(p).is_err()
                || (*m == scion_core::Mode::Symlink && (c.is_empty() || c.contains(&0)))
        }) {
            continue;
        }
        // APFS (macOS) refuses file names that are not valid UTF-8 (EILSEQ).
        if cfg!(target_os = "macos") && f.keys().any(|p| std::str::from_utf8(p).is_err()) {
            eprintln!("SKIP: edge case {name}: non-UTF-8 names unsupported by this filesystem");
            continue;
        }
        // Very deep nesting exceeds PATH_MAX for std::fs based snapshots: see the dedicated
        // test below.
        if f.keys()
            .any(|p| p.iter().filter(|b| **b == b'/').count() > 64)
        {
            continue;
        }
        let paths: Vec<RepoPath> = f.keys().map(|p| RepoPath::parse(p).unwrap()).collect();
        if !scion_wc::fs_safe::case_collisions(&paths).is_empty() {
            continue; // covered by checkout_safety (reported, not written)
        }
        let refname = format!("edge-{name}");
        if scion_object::RefName::parse(&refname).is_err() {
            continue;
        }
        env.commit(&f, &refname);
        let snap = scion_wc::testing::checkout_snapshot(&s, &refname, &[])
            .await
            .unwrap_or_else(|e| panic!("edge case {name}: {e}"));
        assert_eq!(snapshot_files(&snap), f, "edge case {name}");
    }
}

#[tokio::test]
async fn checkout_records_state_and_tracking_ref() {
    let env = Env::start().await;
    let f = files(&[("a.txt", 1, "a\n"), ("bin/run", 2, "#!/bin/sh\n")]);
    let cs = env.commit(&f, "main");
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
    assert_eq!(wc.state().base, cs);
    assert_eq!(
        wc.state().tracking_ref.as_ref().map(|r| r.as_str()),
        Some("main")
    );
    assert_eq!(wc.state().sparse, vec![RepoPath::root()]);
    assert!(wc.state().pending.is_none());
    let reopened = WorkingCopy::open(&root, env.client.clone(), WcOptions::default()).unwrap();
    assert_eq!(reopened.state(), wc.state());

    // By changeset id: no tracking ref.
    let dir2 = tmp();
    let wc2 = WorkingCopy::checkout(
        env.client.clone(),
        &dir2.path().join("wc"),
        &cs.to_text(),
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(wc2.state().base, cs);
    assert!(wc2.state().tracking_ref.is_none());

    // Checking out into an existing working copy is refused.
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
    assert_eq!(err.code, scion_core::ErrorCode::InvalidArgument);
}

#[test]
fn deeply_nested_paths_check_out_through_safe_root() {
    // Thousands of nested directories: run with a large stack (debug builds recurse in the
    // test encoder and the status walk).
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(deep_case())
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn deep_case() {
    let env = Env::start().await;
    let Some((_, repo)) = scion_testkit::edge_cases()
        .into_iter()
        .find(|(n, _)| *n == "deep_nesting_4096")
    else {
        eprintln!("SKIP: no deep_nesting_4096 edge case");
        return;
    };
    // The commit holding the deepest tree.
    let Some(commit) = repo
        .commits
        .iter()
        .max_by_key(|c| c.tree.keys().map(|p| p.len()).max().unwrap_or(0))
    else {
        return;
    };
    let f = from_triples(
        commit
            .tree
            .iter()
            .map(|(p, m)| (p.clone(), m.mode, m.content.clone())),
    );
    env.commit(&f, "deep");
    let dir = tmp();
    let root = dir.path().join("wc");
    let mut wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "deep",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .unwrap();
    let safe = scion_wc::fs_safe::SafeRoot::open(&root).unwrap();
    for (p, (m, c)) in &f {
        let rp = RepoPath::parse(p).unwrap();
        let got = if *m == scion_core::Mode::Symlink {
            safe.read_link(&rp).unwrap()
        } else {
            safe.read(&rp).unwrap()
        };
        assert_eq!(&got, c);
    }
    assert!(wc.status().await.unwrap().is_clean());
}
