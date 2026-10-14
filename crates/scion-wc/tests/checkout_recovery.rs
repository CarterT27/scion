//! SPEC 11.3 / 19.10 / 16.1 class 8: a crash at `wc.checkout.mid_write` leaves a pending
//! Checkout that the next command completes; plus the reusable scenario over every
//! materialization fault point (P18).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod checkout_support;

use checkout_support::*;
use scion_core::{ErrorCode, FaultAction, FaultRegistry};
use scion_wc::points;
use scion_wc::state::PendingOperation;
use scion_wc::{WcOptions, WorkingCopy};

fn tree() -> Files {
    files(&[
        ("a.txt", 1, "a\n"),
        ("b/c.txt", 1, "c\n"),
        ("b/d.sh", 2, "#!/bin/sh\n"),
        ("e/link", 3, "../a.txt"),
        ("e/f/g.txt", 1, "g\n"),
    ])
}

#[tokio::test]
async fn crash_mid_checkout_is_completed_by_the_next_command() {
    for action in [
        FaultAction::Crash,
        FaultAction::Error(ErrorCode::Unavailable),
    ] {
        let env = Env::start().await;
        let f = tree();
        let cs = env.commit(&f, "main");
        let faults = FaultRegistry::new();
        let opts = WcOptions {
            faults: faults.clone(),
            fetch_batch: 2,
            ..WcOptions::default()
        };
        faults.arm(points::CHECKOUT_MID_WRITE, action, Some(1));
        let dir = tmp();
        let root = dir.path().join("wc");
        let err =
            WorkingCopy::checkout(env.client.clone(), &root, "main", Vec::new(), opts.clone())
                .await
                .err()
                .expect("the armed fault fails the checkout");
        assert_eq!(err.code, ErrorCode::Unavailable);
        let st = scion_wc::state::load(&root.join(".scion")).unwrap();
        assert_eq!(st.pending, Some(PendingOperation::Checkout { target: cs }));
        let partial = scion_wc::testing::snapshot(&root).unwrap();
        assert!(partial.len() < f.len(), "the crash happened mid-write");
        faults.reset();
        faults.disarm_all();

        let mut wc = WorkingCopy::open(&root, env.client.clone(), opts).unwrap();
        let recovered = wc.recover().await.unwrap();
        assert!(recovered.unwrap().contains("checkout"));
        assert!(wc.state().pending.is_none());
        let snap = scion_wc::testing::snapshot(&root).unwrap();
        assert_eq!(snapshot_files(&snap), f);
        assert!(wc.status().await.unwrap().is_clean());
    }
}

#[tokio::test]
async fn crash_mid_checkout_then_status_completes_it() {
    let env = Env::start().await;
    let f = tree();
    env.commit(&f, "main");
    let faults = FaultRegistry::new();
    let opts = WcOptions {
        faults: faults.clone(),
        ..WcOptions::default()
    };
    faults.arm(points::CHECKOUT_MID_WRITE, FaultAction::Crash, Some(1));
    let dir = tmp();
    let root = dir.path().join("wc");
    assert!(
        WorkingCopy::checkout(env.client.clone(), &root, "main", Vec::new(), opts.clone())
            .await
            .is_err()
    );
    faults.reset();
    let mut wc = WorkingCopy::open(&root, env.client.clone(), opts).unwrap();
    let r = wc.status().await.unwrap();
    assert!(r.is_clean(), "{r:?}");
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    assert_eq!(snapshot_files(&snap), f);
}

#[tokio::test]
async fn every_materialization_point_scenario_recovers() {
    let env = Env::start().await;
    env.commit(&tree(), "v1");
    let mut f2 = tree();
    f2.remove(b"a.txt".as_slice());
    f2.insert(
        b"b/new.txt".to_vec(),
        (scion_core::Mode::File, b"new\n".to_vec()),
    );
    env.commit(&f2, "v2");
    let s = env.scenario();
    for p in scion_wc::testing::materialization_points() {
        if p.starts_with("wc.goto.") {
            scion_wc::testing::goto_crash_recovery(&s, "v1", "v2", p)
                .await
                .unwrap_or_else(|e| panic!("{p}: {e}"));
        }
    }
}
