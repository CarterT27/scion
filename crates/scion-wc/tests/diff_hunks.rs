//! `WorkingCopy::diff_with_hunks` against the real in-process server: text changes carry
//! exactly `scion_merge::unified_hunks` of the base and disk bytes, binary changes report
//! `binary` with no hunks, and `diff` keeps its id-only listing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod commit_support;

use commit_support::*;
use scion_core::Mode;
use scion_wc::testing::{checkout_at, remove_files, seed_files, write_files};
use scion_wc::{FileChangeView, HunkView};

fn by_path<'a>(v: &'a [FileChangeView], p: &str) -> &'a FileChangeView {
    v.iter().find(|c| c.path == p).unwrap()
}

fn expected(a: &[u8], b: &[u8], context: usize) -> Vec<HunkView> {
    scion_merge::unified_hunks(a, b, context)
        .into_iter()
        .map(HunkView::from)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn hunks_equal_unified_hunks_and_binary_has_none() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let r = main_ref();
    let old_text: &[u8] = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
    let new_text: &[u8] = b"one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine";
    let old_bin: &[u8] = b"bin\0ary\n";
    let new_bin: &[u8] = b"bin\0ary changed\n";
    let latin: &[u8] = b"caf\xe9\n";
    seed_files(
        &s,
        &r,
        &[
            ("text.txt", Mode::File, old_text),
            ("data.bin", Mode::File, old_bin),
            ("gone.txt", Mode::File, b"bye\n"),
            ("run.sh", Mode::File, b"#!/bin/sh\n"),
            ("latin.txt", Mode::File, latin),
        ],
        &[],
    )
    .await
    .unwrap();
    let mut wc = checkout_at(&s, r.as_str(), None).await.unwrap();
    write_files(
        &wc,
        &[
            ("text.txt", Mode::File, new_text),
            ("data.bin", Mode::File, new_bin),
            ("new.txt", Mode::File, b"hello\n"),
            ("run.sh", Mode::Executable, b"#!/bin/sh\n"),
            ("latin.txt", Mode::File, b"cafe\n"),
        ],
    )
    .unwrap();
    remove_files(&wc, &["gone.txt"]).unwrap();

    let plain = wc.diff().await.unwrap();
    assert!(plain.iter().all(|c| c.hunks.is_none() && !c.binary));

    for context in [3usize, 0, 1] {
        let d = wc.diff_with_hunks(context).await.unwrap();
        let paths: Vec<&str> = d.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "data.bin",
                "gone.txt",
                "latin.txt",
                "new.txt",
                "run.sh",
                "text.txt"
            ]
        );
        // Same listing as `diff` apart from the new fields.
        for (a, b) in d.iter().zip(&plain) {
            assert_eq!(
                (&a.path, &a.status, &a.old_id, &a.new_id),
                (&b.path, &b.status, &b.old_id, &b.new_id)
            );
        }
        let t = by_path(&d, "text.txt");
        assert!(!t.binary);
        assert_eq!(
            t.hunks.as_ref().unwrap(),
            &expected(old_text, new_text, context)
        );
        let bin = by_path(&d, "data.bin");
        assert!(bin.binary);
        assert!(bin.hunks.is_none());
        let n = by_path(&d, "new.txt");
        assert_eq!(n.status, "added");
        assert_eq!(
            n.hunks.as_ref().unwrap(),
            &expected(b"", b"hello\n", context)
        );
        let g = by_path(&d, "gone.txt");
        assert_eq!(g.status, "deleted");
        assert_eq!(g.hunks.as_ref().unwrap(), &expected(b"bye\n", b"", context));
        let m = by_path(&d, "run.sh");
        assert_eq!(m.status, "mode_changed");
        assert_eq!(m.hunks.as_deref(), Some(&[][..]));
        let l = by_path(&d, "latin.txt");
        let hunks = l.hunks.as_ref().unwrap();
        let del = &hunks[0].lines[0];
        assert_eq!(del.kind, "delete");
        assert_eq!(del.text_hex.as_deref(), Some(hex::encode(latin).as_str()));
        assert_eq!(del.bytes(), latin);
        assert_eq!(hunks[0].lines[1].text_hex, None);
        assert_eq!(hunks[0].lines[1].text, "cafe\n");
    }
}
