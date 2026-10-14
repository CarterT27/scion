//! SPEC 11.7 / 18.3 rebase with structural (file/directory) changes against the real
//! in-process server (P28): a member that replaces a directory by a file, and
//! `file_directory` conflicts in both directions, are materialized (clean deletes applied,
//! ours kept, the other side saved under `.scion/conflicts`), resolved and committed.

mod commit_support;

use commit_support::*;
use scion_core::Mode;
use scion_wc::testing::{checkout_at, checkout_snapshot, seed_files, wc_snapshot, write_files};

#[tokio::test(flavor = "multi_thread")]
async fn directory_replaced_by_file_conflicts_and_resolves() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let r = main_ref();
    let start = seed_files(
        &s,
        &r,
        &[("d/a", Mode::File, b"a\n"), ("c.txt", Mode::File, b"c0\n")],
        &[],
    )
    .await
    .unwrap();
    let mut wc = checkout_at(&s, &start.to_text(), None).await.unwrap();
    seed_files(&s, &r, &[("c.txt", Mode::File, b"up\n")], &[])
        .await
        .unwrap();
    std::fs::remove_file(wc.root().join("d/a")).unwrap();
    std::fs::remove_dir(wc.root().join("d")).unwrap();
    write_files(
        &wc,
        &[
            ("d", Mode::File, b"file d\n"),
            ("c.txt", Mode::File, b"local\n"),
        ],
    )
    .unwrap();
    let original = wc.commit_with_report(b"m\n", b"A", None).await.unwrap();
    let report = wc.rebase("main").await.unwrap();
    // SPEC 18.3: onto still has the directory `d`, so the file `d` is a file_directory
    // conflict; the member's delete of `d/a` is a clean entry and is applied.
    assert_eq!(report.conflicts, vec!["c.txt".to_owned(), "d".to_owned()]);
    let disk = wc_snapshot(&wc).unwrap();
    assert!(!disk.contains_key(&rp("d/a")));
    assert!(!disk.contains_key(&rp("d")));
    assert_eq!(
        std::fs::read(wc.conflict_side_path(&rp("d"), "theirs")).unwrap(),
        b"file d\n"
    );
    // Resolve by taking the local file (an emptied directory may be left behind).
    let _ = std::fs::remove_dir(wc.root().join("d"));
    write_files(
        &wc,
        &[
            ("d", Mode::File, b"file d\n"),
            ("c.txt", Mode::File, b"both\n"),
        ],
    )
    .unwrap();
    wc.resolve(&rp("d"), false).unwrap();
    wc.resolve(&rp("c.txt"), false).unwrap();
    let cont = wc.commit_with_report(b"", b"", None).await.unwrap();
    assert_eq!(cont.change_id, original.change_id);
    assert!(wc.state().pending.is_none());
    assert!(wc.status().await.unwrap().is_clean());
    let disk = wc_snapshot(&wc).unwrap();
    assert_eq!(
        disk,
        checkout_snapshot(&s, &wc.state().base.to_text(), &[])
            .await
            .unwrap()
    );
    assert_eq!(disk[&rp("c.txt")].1, b"both\n");
    assert_eq!(disk[&rp("d")], (Mode::File, b"file d\n".to_vec()));
    assert!(!disk.contains_key(&rp("d/a")));
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn file_directory_conflicts_are_materialized_and_resolvable() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let r = main_ref();
    let start = seed_files(&s, &r, &[("k.txt", Mode::File, b"k\n")], &[])
        .await
        .unwrap();
    let mut wc = checkout_at(&s, &start.to_text(), None).await.unwrap();
    // Upstream: a file where the local side puts a directory, and a directory where the
    // local side puts a file.
    seed_files(
        &s,
        &r,
        &[
            ("x", Mode::File, b"up x\n"),
            ("y/b", Mode::File, b"up y/b\n"),
        ],
        &[],
    )
    .await
    .unwrap();
    write_files(
        &wc,
        &[
            ("x/a", Mode::File, b"local x/a\n"),
            ("y", Mode::File, b"local y\n"),
        ],
    )
    .unwrap();
    let original = wc.commit_with_report(b"m\n", b"A", None).await.unwrap();
    let report = wc.rebase("main").await.unwrap();
    assert!(!report.conflicts.is_empty(), "{report:?}");
    assert!(wc.state().pending.is_some());
    // The ours (upstream) versions are on disk and the local sides are saved.
    let disk = wc_snapshot(&wc).unwrap();
    assert_eq!(disk[&rp("x")].1, b"up x\n");
    assert_eq!(disk[&rp("y/b")].1, b"up y/b\n");
    for c in &report.conflicts {
        let theirs = std::fs::read(wc.conflict_side_path(&rp(c), "theirs")).unwrap();
        assert!(theirs.starts_with(b"local"), "{c}");
    }
    // Resolve: keep upstream's files and move the local content beside them.
    write_files(
        &wc,
        &[
            ("x2/a", Mode::File, b"local x/a\n"),
            ("y2", Mode::File, b"local y\n"),
        ],
    )
    .unwrap();
    for c in &report.conflicts {
        wc.resolve(&rp(c), false).unwrap();
    }
    let cont = wc.commit_with_report(b"", b"", None).await.unwrap();
    assert_eq!(cont.change_id, original.change_id);
    assert!(wc.state().pending.is_none());
    assert!(wc.status().await.unwrap().is_clean());
    assert_eq!(
        wc_snapshot(&wc).unwrap(),
        checkout_snapshot(&s, &wc.state().base.to_text(), &[])
            .await
            .unwrap()
    );
    validate_server(&srv).await;
}
