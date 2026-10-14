//! SPEC 19.10 / 11.7 rebase against the real in-process server (P28): the full conflict
//! flow, clean multi-member stacks, the dirty refusal, fast-forward, resolve errors, a
//! conflict outside the sparse scope, and crashes while continuing a paused rebase.

mod commit_support;

use commit_support::*;
use scion_core::{ErrorCode, FaultAction, FaultRegistry, Mode};
use scion_wc::points;
use scion_wc::state::PendingOperation;
use scion_wc::testing::{
    checkout_at, checkout_snapshot, conflict_flow, ref_target, seed_files, wc_snapshot, write_files,
};
use scion_wc::{WcOptions, WorkingCopy};

#[tokio::test(flavor = "multi_thread")]
async fn full_conflict_flow() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    conflict_flow(&s, &main_ref()).await.unwrap();
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn clean_stack_rebase_keeps_order_and_change_ids() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let r = main_ref();
    let start = seed_files(
        &s,
        &r,
        &[("a", Mode::File, b"a\n"), ("b", Mode::File, b"b\n")],
        &[],
    )
    .await
    .unwrap();
    let mut wc = checkout_at(&s, &start.to_text(), None).await.unwrap();
    write_files(&wc, &[("a", Mode::File, b"a1\n")]).unwrap();
    let m1 = wc.commit_with_report(b"m1\n", b"A", None).await.unwrap();
    write_files(&wc, &[("b", Mode::File, b"b1\n")]).unwrap();
    let m2 = wc.commit_with_report(b"m2\n", b"A", None).await.unwrap();
    // A member whose change is already upstream is dropped (EMPTY_CHANGE).
    write_files(&wc, &[("c", Mode::File, b"same\n")]).unwrap();
    wc.commit(b"m3\n", b"A", None).await.unwrap();
    seed_files(
        &s,
        &r,
        &[("u", Mode::File, b"u\n"), ("c", Mode::File, b"same\n")],
        &[],
    )
    .await
    .unwrap();
    let onto = ref_target(&s.client, &r).await.unwrap();
    let report = wc.rebase("main").await.unwrap();
    assert!(report.conflicts.is_empty());
    assert_eq!(report.rebased.len(), 2, "{report:?}");
    assert_eq!(report.head.as_deref(), Some(report.rebased[1].as_str()));
    assert_eq!(wc.state().base.to_text(), report.rebased[1]);
    assert!(wc.state().pending.is_none());
    let disk = wc_snapshot(&wc).unwrap();
    assert_eq!(
        disk,
        checkout_snapshot(&s, &report.rebased[1], &[])
            .await
            .unwrap()
    );
    assert_eq!(disk[&rp("a")].1, b"a1\n");
    assert_eq!(disk[&rp("b")].1, b"b1\n");
    assert_eq!(disk[&rp("u")].1, b"u\n");
    // Rebasing again onto the same destination is a no-op that keeps the stack.
    let again = wc.rebase("main").await.unwrap();
    assert_eq!(again.rebased, report.rebased);
    // The stack lands as two members with their change ids.
    let landed = wc.land(&r).await.unwrap();
    assert_eq!(landed.landed, report.rebased);
    let _ = (m1, m2, onto);
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rebase_refuses_dirty_and_fast_forwards() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let r = main_ref();
    let start = seed_files(&s, &r, &[("a", Mode::File, b"a\n")], &[])
        .await
        .unwrap();
    let mut wc = checkout_at(&s, &start.to_text(), None).await.unwrap();
    seed_files(&s, &r, &[("b", Mode::File, b"b\n")], &[])
        .await
        .unwrap();
    write_files(&wc, &[("untracked", Mode::File, b"u\n")]).unwrap();
    let e = wc.rebase("main").await.unwrap_err();
    assert_eq!(e.subcode(), Some(WorkingCopy::SUBCODE_DIRTY));
    assert!(wc.state().pending.is_none());
    std::fs::remove_file(wc.root().join("untracked")).unwrap();
    // The base is an ancestor of onto: an empty stack just moves to onto.
    let report = wc.rebase("main").await.unwrap();
    let tip = ref_target(&s.client, &r).await.unwrap();
    assert!(report.rebased.is_empty());
    assert_eq!(report.head, Some(tip.to_text()));
    assert_eq!(wc.state().base, tip);
    assert_eq!(wc_snapshot(&wc).unwrap()[&rp("b")].1, b"b\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_and_abort_errors() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let mut wc = checkout_at(&s, "main", None).await.unwrap();
    let e = wc.resolve(&rp("x"), false).unwrap_err();
    assert_eq!(e.subcode(), Some(WorkingCopy::SUBCODE_NO_REBASE));
    let e = wc.rebase_abort().await.unwrap_err();
    assert_eq!(e.subcode(), Some(WorkingCopy::SUBCODE_NO_REBASE));

    let r = main_ref();
    let start = seed_files(&s, &r, &[("t", Mode::File, b"1\n2\n3\n")], &[])
        .await
        .unwrap();
    let mut wc = checkout_at(&s, &start.to_text(), None).await.unwrap();
    seed_files(&s, &r, &[("t", Mode::File, b"1\nup\n3\n")], &[])
        .await
        .unwrap();
    write_files(&wc, &[("t", Mode::File, b"1\nlocal\n3\n")]).unwrap();
    wc.commit(b"local\n", b"A", None).await.unwrap();
    let report = wc.rebase("main").await.unwrap();
    assert_eq!(report.conflicts, vec!["t".to_owned()]);
    let e = wc.resolve(&rp("other"), false).unwrap_err();
    assert_eq!(e.subcode(), Some(WorkingCopy::SUBCODE_NOT_CONFLICTED));
    // Other mutating commands are refused while paused.
    let e = wc.rebase("main").await.unwrap_err();
    assert_eq!(e.subcode(), Some(WorkingCopy::SUBCODE_REBASE_IN_PROGRESS));
    let e = wc.amend(None).await.unwrap_err();
    assert_eq!(e.subcode(), Some(WorkingCopy::SUBCODE_REBASE_IN_PROGRESS));
    let e = wc.land(&r).await.unwrap_err();
    assert_eq!(e.subcode(), Some(WorkingCopy::SUBCODE_REBASE_IN_PROGRESS));
    let e = wc.commit(b"m", b"A", Some(&[rp("t")])).await.unwrap_err();
    assert_eq!(e.subcode(), Some(WorkingCopy::SUBCODE_REBASE_IN_PROGRESS));
    // Status works while paused and reports the conflicted file.
    let st = wc.status().await.unwrap();
    assert_eq!(st.modified, vec![rp("t")]);
    assert!(matches!(
        wc.state().pending,
        Some(PendingOperation::Rebase { .. })
    ));
    // A reopened handle sees the same paused rebase; deleting the file resolves it.
    let root = wc.root().to_path_buf();
    drop(wc);
    let mut wc = WorkingCopy::open(&root, s.client.clone(), WcOptions::default()).unwrap();
    assert!(wc.recover().await.unwrap().is_none());
    std::fs::remove_file(root.join("t")).unwrap();
    wc.resolve(&rp("t"), false).unwrap();
    let c = wc.commit_with_report(b"", b"", None).await.unwrap();
    assert_eq!(c.delta.len(), 1);
    assert!(wc.state().pending.is_none());
    assert!(!wc_snapshot(&wc).unwrap().contains_key(&rp("t")));
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resolving_to_onto_drops_the_member() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let r = main_ref();
    let start = seed_files(&s, &r, &[("t", Mode::File, b"1\n2\n3\n")], &[])
        .await
        .unwrap();
    let mut wc = checkout_at(&s, &start.to_text(), None).await.unwrap();
    seed_files(&s, &r, &[("t", Mode::File, b"1\nup\n3\n")], &[])
        .await
        .unwrap();
    let tip = ref_target(&s.client, &r).await.unwrap();
    write_files(&wc, &[("t", Mode::File, b"1\nlocal\n3\n")]).unwrap();
    wc.commit(b"local\n", b"A", None).await.unwrap();
    wc.rebase("main").await.unwrap();
    write_files(&wc, &[("t", Mode::File, b"1\nup\n3\n")]).unwrap();
    wc.resolve(&rp("t"), false).unwrap();
    let c = wc.commit_with_report(b"", b"", None).await.unwrap();
    assert!(c.delta.is_empty());
    assert_eq!(wc.state().base, tip);
    assert!(wc.state().pending.is_none());
    assert!(wc.status().await.unwrap().is_clean());
}

#[tokio::test(flavor = "multi_thread")]
async fn conflict_outside_sparse_scope_aborts() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let r = main_ref();
    let start = seed_files(
        &s,
        &r,
        &[
            ("in/a", Mode::File, b"a\n"),
            ("out/t", Mode::File, b"1\n2\n3\n"),
        ],
        &[],
    )
    .await
    .unwrap();
    // A full working copy creates a change in out/ and in/.
    let mut full = checkout_at(&s, &start.to_text(), None).await.unwrap();
    write_files(
        &full,
        &[
            ("out/t", Mode::File, b"1\nlocal\n3\n"),
            ("in/a", Mode::File, b"a2\n"),
        ],
    )
    .unwrap();
    let local = full.commit(b"local\n", b"A", None).await.unwrap();
    seed_files(&s, &r, &[("out/t", Mode::File, b"1\nup\n3\n")], &[])
        .await
        .unwrap();
    // A sparse working copy of only in/ at that changeset cannot materialize the conflict.
    let mut wc = WorkingCopy::checkout(
        s.client.clone(),
        &s.fresh_dir("sparse"),
        &local.to_text(),
        vec![rp("in")],
        WcOptions::default(),
    )
    .await
    .unwrap();
    let before = wc_snapshot(&wc).unwrap();
    let e = wc.rebase("main").await.unwrap_err();
    assert_eq!(e.subcode(), Some(WorkingCopy::SUBCODE_OUT_OF_SCOPE));
    assert_eq!(wc.state().base, local);
    assert!(wc.state().pending.is_none());
    assert_eq!(wc_snapshot(&wc).unwrap(), before);
    // The full working copy rebases with the conflict materialized.
    let report = full.rebase("main").await.unwrap();
    assert_eq!(report.conflicts, vec!["out/t".to_owned()]);
    full.rebase_abort().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn sparse_rebase_carries_out_of_scope_clean_entries() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let r = main_ref();
    let start = seed_files(
        &s,
        &r,
        &[
            ("in/t", Mode::File, b"1\n2\n3\n"),
            ("out/o", Mode::File, b"o\n"),
        ],
        &[],
    )
    .await
    .unwrap();
    let mut full = checkout_at(&s, &start.to_text(), None).await.unwrap();
    write_files(
        &full,
        &[
            ("in/t", Mode::File, b"1\nlocal\n3\n"),
            ("out/o", Mode::File, b"o2\n"),
        ],
    )
    .unwrap();
    let local = full.commit(b"local\n", b"A", None).await.unwrap();
    seed_files(&s, &r, &[("in/t", Mode::File, b"1\nup\n3\n")], &[])
        .await
        .unwrap();
    let mut wc = WorkingCopy::checkout(
        s.client.clone(),
        &s.fresh_dir("sparse"),
        &local.to_text(),
        vec![rp("in")],
        WcOptions::default(),
    )
    .await
    .unwrap();
    let report = wc.rebase("main").await.unwrap();
    assert_eq!(report.conflicts, vec!["in/t".to_owned()]);
    write_files(&wc, &[("in/t", Mode::File, b"1\nboth\n3\n")]).unwrap();
    wc.resolve(&rp("in/t"), false).unwrap();
    let c = wc.commit_with_report(b"", b"", None).await.unwrap();
    let paths: Vec<_> = c.delta.iter().map(|d| d.path.clone()).collect();
    assert_eq!(paths, vec![rp("in/t"), rp("out/o")]);
    let fresh = checkout_snapshot(&s, &c.changeset.to_text(), &[])
        .await
        .unwrap();
    assert_eq!(fresh[&rp("out/o")].1, b"o2\n");
    assert_eq!(fresh[&rp("in/t")].1, b"1\nboth\n3\n");
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_while_continuing_is_recoverable() {
    for point in [
        points::COMMIT_AFTER_UPLOAD,
        points::COMMIT_AFTER_CREATE,
        points::REBASE_MID_MEMBER,
    ] {
        let srv = server().await;
        let s = scenario(&srv, &alice()).await;
        let r = main_ref();
        let start = seed_files(
            &s,
            &r,
            &[("t", Mode::File, b"1\n2\n3\n"), ("u", Mode::File, b"u\n")],
            &[],
        )
        .await
        .unwrap();
        let faults = FaultRegistry::new();
        let mut wc = checkout_at(&s, &start.to_text(), Some(faults.clone()))
            .await
            .unwrap();
        seed_files(&s, &r, &[("t", Mode::File, b"1\nup\n3\n")], &[])
            .await
            .unwrap();
        write_files(&wc, &[("t", Mode::File, b"1\nlocal\n3\n")]).unwrap();
        let m1 = wc.commit_with_report(b"m1\n", b"A", None).await.unwrap();
        write_files(&wc, &[("u", Mode::File, b"u2\n")]).unwrap();
        let m2 = wc.commit_with_report(b"m2\n", b"A", None).await.unwrap();
        let report = wc.rebase("main").await.unwrap();
        assert_eq!(report.conflicts, vec!["t".to_owned()]);
        write_files(&wc, &[("t", Mode::File, b"1\nresolved\n3\n")]).unwrap();
        wc.resolve(&rp("t"), false).unwrap();
        faults.arm(point, FaultAction::Crash, Some(1));
        assert!(wc.commit(b"", b"", None).await.is_err(), "{point}");
        faults.reset();
        let root = wc.root().to_path_buf();
        drop(wc);
        let opts = WcOptions {
            faults,
            ..WcOptions::default()
        };
        let mut wc = WorkingCopy::open(&root, s.client.clone(), opts).unwrap();
        wc.status().await.unwrap();
        if wc.state().pending.is_some() {
            // Still paused (the crash hit before the journal moved on): commit again.
            wc.commit(b"", b"", None).await.unwrap();
        }
        assert!(wc.state().pending.is_none(), "{point}");
        let disk = wc_snapshot(&wc).unwrap();
        assert_eq!(disk[&rp("t")].1, b"1\nresolved\n3\n");
        assert_eq!(disk[&rp("u")].1, b"u2\n");
        assert_eq!(
            disk,
            checkout_snapshot(&s, &wc.state().base.to_text(), &[])
                .await
                .unwrap()
        );
        let top = scion_wc::testing::ref_target(&s.client, &r).await.unwrap();
        assert_ne!(top, wc.state().base);
        assert!(wc.status().await.unwrap().is_clean());
        let _ = (m1, m2);
        validate_server(&srv).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn abort_of_a_multi_member_stack_after_partial_progress() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let r = main_ref();
    let start = seed_files(
        &s,
        &r,
        &[("a", Mode::File, b"a\n"), ("t", Mode::File, b"1\n2\n3\n")],
        &[],
    )
    .await
    .unwrap();
    let mut wc = checkout_at(&s, &start.to_text(), None).await.unwrap();
    seed_files(&s, &r, &[("t", Mode::File, b"1\nup\n3\n")], &[])
        .await
        .unwrap();
    write_files(&wc, &[("a", Mode::File, b"a1\n")]).unwrap();
    wc.commit(b"m1\n", b"A", None).await.unwrap();
    write_files(
        &wc,
        &[
            ("t", Mode::File, b"1\nlocal\n3\n"),
            ("n", Mode::Executable, b"#!\n"),
        ],
    )
    .unwrap();
    let original = wc.commit(b"m2\n", b"A", None).await.unwrap();
    let original_snap = wc_snapshot(&wc).unwrap();
    let report = wc.rebase("main").await.unwrap();
    assert_eq!(report.rebased.len(), 1);
    assert_eq!(report.conflicts, vec!["t".to_owned()]);
    assert_eq!(wc.state().base.to_text(), report.rebased[0]);
    wc.rebase_abort().await.unwrap();
    assert_eq!(wc.state().base, original);
    assert_eq!(wc_snapshot(&wc).unwrap(), original_snap);
    assert!(wc.status().await.unwrap().is_clean());
    let e = wc.rebase_abort().await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
}
