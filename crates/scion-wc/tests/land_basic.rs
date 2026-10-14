//! SPEC 19.10 / 11.7 land from a working copy against the real in-process server (P28):
//! the base moves to the landed result, the tracking ref is set, unrelated dirty files are
//! preserved, and dirty files in the way keep the working copy at its old base.

mod commit_support;

use commit_support::*;
use scion_core::{ErrorCode, Mode};
use scion_wc::testing::{
    checkout_at, land_moves_base, ref_target, seed_files, wc_snapshot, write_files,
};

#[tokio::test(flavor = "multi_thread")]
async fn land_moves_base_to_the_landed_result() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    land_moves_base(&s, &main_ref()).await.unwrap();
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn land_without_concurrent_change_is_the_committed_changeset() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let r = main_ref();
    let mut wc = checkout_at(&s, "main", None).await.unwrap();
    write_files(&wc, &[("x", Mode::File, b"x\n")]).unwrap();
    let c = wc.commit(b"x\n", b"A", None).await.unwrap();
    let report = wc.land(&r).await.unwrap();
    assert_eq!(report.landed, vec![c.to_text()]);
    assert_eq!(report.version, 2);
    assert_eq!(wc.state().base, c);
    assert_eq!(ref_target(&s.client, &r).await.unwrap(), c);
    assert!(wc.status().await.unwrap().is_clean());
}

#[tokio::test(flavor = "multi_thread")]
async fn land_preserves_unrelated_dirty_files_and_stops_for_touched_ones() {
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
    // Unrelated dirty file: preserved, the base moves.
    let mut wc = checkout_at(&s, &start.to_text(), None).await.unwrap();
    write_files(&wc, &[("mine", Mode::File, b"m\n")]).unwrap();
    wc.commit(b"mine\n", b"A", None).await.unwrap();
    seed_files(&s, &r, &[("theirs", Mode::File, b"t\n")], &[])
        .await
        .unwrap();
    write_files(&wc, &[("a", Mode::File, b"dirty a\n")]).unwrap();
    let report = wc.land(&r).await.unwrap();
    assert!(report.moved);
    let tip = ref_target(&s.client, &r).await.unwrap();
    assert_eq!(wc.state().base, tip);
    let disk = wc_snapshot(&wc).unwrap();
    assert_eq!(disk[&rp("a")].1, b"dirty a\n");
    assert_eq!(disk[&rp("theirs")].1, b"t\n");
    assert_eq!(wc.status().await.unwrap().modified, vec![rp("a")]);

    // A dirty file the landed result changes: the working copy stays at its old base.
    let mut wc2 = checkout_at(&s, r.as_str(), None).await.unwrap();
    write_files(&wc2, &[("c", Mode::File, b"c\n")]).unwrap();
    let mine = wc2.commit(b"c\n", b"A", None).await.unwrap();
    seed_files(&s, &r, &[("b", Mode::File, b"b upstream\n")], &[])
        .await
        .unwrap();
    write_files(&wc2, &[("b", Mode::File, b"b dirty\n")]).unwrap();
    let report = wc2.land(&r).await.unwrap();
    assert!(!report.moved);
    assert_eq!(report.head, mine.to_text());
    assert_eq!(wc2.state().base, mine);
    assert!(wc2.state().pending.is_none());
    assert_eq!(wc2.state().tracking_ref.as_ref(), Some(&r));
    assert_eq!(wc_snapshot(&wc2).unwrap()[&rp("b")].1, b"b dirty\n");
    // After discarding the edit, landing again (idempotent) moves the working copy.
    write_files(&wc2, &[("b", Mode::File, b"b\n")]).unwrap();
    let again = wc2.land(&r).await.unwrap();
    assert!(again.moved);
    let tip = ref_target(&s.client, &r).await.unwrap();
    assert_eq!(wc2.state().base, tip);
    assert_eq!(wc_snapshot(&wc2).unwrap()[&rp("b")].1, b"b upstream\n");
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn land_conflict_is_reported_and_changes_nothing() {
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
    write_files(&wc, &[("t", Mode::File, b"1\nlocal\n3\n")]).unwrap();
    let c = wc.commit(b"local\n", b"A", None).await.unwrap();
    let e = wc.land(&r).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::MergeConflict);
    assert_eq!(wc.state().base, c);
    assert!(wc.state().pending.is_none());
    // The documented recovery: rebase, resolve, commit, land.
    let report = wc.rebase("main").await.unwrap();
    assert_eq!(report.conflicts, vec!["t".to_owned()]);
    write_files(&wc, &[("t", Mode::File, b"1\nmerged\n3\n")]).unwrap();
    wc.resolve(&rp("t"), false).unwrap();
    wc.commit(b"", b"", None).await.unwrap();
    let landed = wc.land(&r).await.unwrap();
    assert!(landed.moved);
    assert_eq!(wc.state().base, ref_target(&s.client, &r).await.unwrap());
    validate_server(&srv).await;
}
