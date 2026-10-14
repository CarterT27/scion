//! Every row of the SPEC 8.3 table and both structural rules, on hand examples (SPEC 19.4).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use scion_refmodel::{MConflict, MFile, MPath, MTree, merge_trees};
use std::collections::BTreeSet;

const MAX: u64 = 10_485_760;

fn f(mode: u8, c: &str) -> MFile {
    MFile {
        mode,
        content: c.as_bytes().to_vec(),
    }
}

fn tree(entries: &[(&str, MFile)]) -> MTree {
    entries
        .iter()
        .map(|(p, f)| (p.as_bytes().to_vec(), f.clone()))
        .collect()
}

/// One-path trees: `None` is absent.
fn one(p: &str, e: Option<MFile>) -> MTree {
    match e {
        Some(e) => tree(&[(p, e)]),
        None => MTree::new(),
    }
}

fn merge1(b: Option<MFile>, o: Option<MFile>, t: Option<MFile>) -> Result<MTree, Vec<MConflict>> {
    merge_trees(
        &one("p", b),
        &one("p", o),
        &one("p", t),
        None,
        MAX,
        &BTreeSet::new(),
    )
}

fn kinds(r: Result<MTree, Vec<MConflict>>) -> Vec<(String, &'static str)> {
    match r {
        Ok(t) => panic!("expected conflicts, got {t:?}"),
        Err(cs) => cs
            .into_iter()
            .map(|c| (String::from_utf8(c.path).unwrap(), c.kind))
            .collect(),
    }
}

fn conflict_p(kind: &'static str) -> Vec<(String, &'static str)> {
    vec![("p".to_string(), kind)]
}

#[test]
fn row_o_equals_b_takes_t() {
    let r = merge1(Some(f(1, "b")), Some(f(1, "b")), Some(f(1, "t"))).unwrap();
    assert_eq!(r, one("p", Some(f(1, "t"))));
    // Including a deletion by t.
    let r = merge1(Some(f(1, "b")), Some(f(1, "b")), None).unwrap();
    assert!(r.is_empty());
}

#[test]
fn row_t_equals_b_takes_o() {
    let r = merge1(Some(f(1, "b")), Some(f(2, "o")), Some(f(1, "b"))).unwrap();
    assert_eq!(r, one("p", Some(f(2, "o"))));
    let r = merge1(Some(f(1, "b")), None, Some(f(1, "b"))).unwrap();
    assert!(r.is_empty());
}

#[test]
fn row_o_equals_t_takes_o() {
    let r = merge1(Some(f(1, "b")), Some(f(1, "x")), Some(f(1, "x"))).unwrap();
    assert_eq!(r, one("p", Some(f(1, "x"))));
    let r = merge1(None, Some(f(3, "x")), Some(f(3, "x"))).unwrap();
    assert_eq!(r, one("p", Some(f(3, "x"))));
    let r = merge1(Some(f(1, "b")), None, None).unwrap();
    assert!(r.is_empty());
}

#[test]
fn row_add_add() {
    assert_eq!(
        kinds(merge1(None, Some(f(1, "x\n")), Some(f(1, "y\n")))),
        conflict_p("add_add")
    );
    // Same content, different mode is still an add/add conflict.
    assert_eq!(
        kinds(merge1(None, Some(f(1, "x")), Some(f(2, "x")))),
        conflict_p("add_add")
    );
}

#[test]
fn row_modify_delete() {
    assert_eq!(
        kinds(merge1(Some(f(1, "b")), None, Some(f(1, "t")))),
        conflict_p("modify_delete")
    );
    assert_eq!(
        kinds(merge1(Some(f(1, "b")), Some(f(1, "o")), None)),
        conflict_p("modify_delete")
    );
}

#[test]
fn row_symlink() {
    assert_eq!(
        kinds(merge1(Some(f(3, "b")), Some(f(3, "o")), Some(f(3, "t")))),
        conflict_p("symlink")
    );
    // Any side a symlink: a file turned into a symlink on one side, edited on the other.
    assert_eq!(
        kinds(merge1(
            Some(f(1, "a\n")),
            Some(f(3, "tgt")),
            Some(f(1, "b\n"))
        )),
        conflict_p("symlink")
    );
    assert_eq!(
        kinds(merge1(
            Some(f(3, "a")),
            Some(f(1, "x\n")),
            Some(f(1, "y\n"))
        )),
        conflict_p("symlink")
    );
}

#[test]
fn row_binary_heuristic_boundary() {
    let mut bin = vec![b'a'; 8001];
    bin[7999] = 0;
    let text_at_8000 = {
        let mut v = vec![b'a'; 8001];
        v[8000] = 0;
        v
    };
    let mk = |c: &[u8]| MFile {
        mode: 1,
        content: c.to_vec(),
    };
    for side in 0..3 {
        let mut s = [mk(b"base\n"), mk(b"ours\n"), mk(b"theirs\n")];
        s[side] = mk(&bin);
        let [b, o, t] = s;
        assert_eq!(
            kinds(merge1(Some(b), Some(o), Some(t))),
            conflict_p("binary"),
            "side {side}"
        );
    }
    // 0x00 at byte 8000 is text: a single-line change on each side conflicts as content.
    let r = merge1(
        Some(mk(b"base\n")),
        Some(mk(&text_at_8000)),
        Some(mk(b"theirs\n")),
    );
    assert_eq!(kinds(r), conflict_p("content"));
}

#[test]
fn row_binary_max_text_boundary() {
    let b = "1\n2\n3\n4\n5\n";
    let o = "X\n2\n3\n4\n5\n";
    let t = "1\n2\n3\n4\nY\n";
    let max = o.len() as u64; // all three sides have the same length
    let at = |m: u64| {
        merge_trees(
            &one("p", Some(f(1, b))),
            &one("p", Some(f(1, o))),
            &one("p", Some(f(1, t))),
            None,
            m,
            &BTreeSet::new(),
        )
    };
    assert_eq!(at(max).unwrap(), one("p", Some(f(1, "X\n2\n3\n4\nY\n"))));
    assert_eq!(kinds(at(max - 1)), conflict_p("binary"));
}

#[test]
fn row_redacted() {
    let red: BTreeSet<Vec<u8>> = [b"secret\n".to_vec()].into();
    let run = |b: &str, o: &str, t: &str| {
        merge_trees(
            &one("p", Some(f(1, b))),
            &one("p", Some(f(1, o))),
            &one("p", Some(f(1, t))),
            None,
            MAX,
            &red,
        )
    };
    assert_eq!(kinds(run("secret\n", "o\n", "t\n")), conflict_p("redacted"));
    assert_eq!(kinds(run("b\n", "secret\n", "t\n")), conflict_p("redacted"));
    assert_eq!(kinds(run("b\n", "o\n", "secret\n")), conflict_p("redacted"));
    // Content not needed: the first rows resolve without reading it.
    assert_eq!(
        run("secret\n", "secret\n", "t\n").unwrap(),
        one("p", Some(f(1, "t\n")))
    );
    assert_eq!(
        run("b\n", "secret\n", "b\n").unwrap(),
        one("p", Some(f(1, "secret\n")))
    );
}

#[test]
fn row_content_clean_and_conflict() {
    let r = merge1(
        Some(f(1, "a\nb\nc\n")),
        Some(f(1, "A\nb\nc\n")),
        Some(f(1, "a\nb\nC\n")),
    )
    .unwrap();
    assert_eq!(r, one("p", Some(f(1, "A\nb\nC\n"))));
    assert_eq!(
        kinds(merge1(
            Some(f(1, "a\nb\nc\n")),
            Some(f(1, "a\nX\nc\n")),
            Some(f(1, "a\nY\nc\n")),
        )),
        conflict_p("content")
    );
}

#[test]
fn row_mode_merge() {
    // ours changes the mode and the content; theirs only the content.
    let r = merge1(
        Some(f(1, "a\nb\nc\n")),
        Some(f(2, "A\nb\nc\n")),
        Some(f(1, "a\nb\nC\n")),
    )
    .unwrap();
    assert_eq!(r, one("p", Some(f(2, "A\nb\nC\n"))));
    // theirs changes the mode.
    let r = merge1(
        Some(f(2, "a\nb\nc\n")),
        Some(f(2, "A\nb\nc\n")),
        Some(f(1, "a\nb\nC\n")),
    )
    .unwrap();
    assert_eq!(r, one("p", Some(f(1, "A\nb\nC\n"))));
    // both change the mode the same way.
    let r = merge1(
        Some(f(1, "a\nb\nc\n")),
        Some(f(2, "A\nb\nc\n")),
        Some(f(2, "a\nb\nC\n")),
    )
    .unwrap();
    assert_eq!(r, one("p", Some(f(2, "A\nb\nC\n"))));
}

#[test]
fn structural_put_over_directory() {
    // t puts a file at p where O has a directory at p.
    let b = MTree::new();
    let o = tree(&[("p/x", f(1, "x"))]);
    let t = tree(&[("p", f(1, "file"))]);
    assert_eq!(
        kinds(merge_trees(&b, &o, &t, None, MAX, &BTreeSet::new())),
        conflict_p("file_directory")
    );
}

#[test]
fn structural_put_under_file() {
    // t puts a file at p where O has a file at an ancestor of p.
    let b = MTree::new();
    let o = tree(&[("d", f(1, "file"))]);
    let t = tree(&[("d/p", f(1, "x"))]);
    assert_eq!(
        kinds(merge_trees(&b, &o, &t, None, MAX, &BTreeSet::new())),
        vec![("d/p".to_string(), "file_directory")]
    );
}

#[test]
fn structural_delete_of_directory() {
    // t deletes p while O has a directory at p.
    let b = tree(&[("p", f(1, "file"))]);
    let o = tree(&[("p/x", f(1, "x"))]);
    let t = MTree::new();
    let paths: BTreeSet<MPath> = [b"p".to_vec()].into();
    assert_eq!(
        kinds(merge_trees(&b, &o, &t, Some(&paths), MAX, &BTreeSet::new())),
        conflict_p("file_directory")
    );
}

#[test]
fn restricted_paths_keep_ours_elsewhere() {
    // Rebase form: only the given paths are examined; everything else is O.
    let b = tree(&[("a", f(1, "1\n")), ("z", f(1, "z\n"))]);
    let o = tree(&[("a", f(1, "1\n")), ("z", f(1, "Z\n")), ("n", f(1, "new\n"))]);
    let t = tree(&[("a", f(1, "2\n")), ("z", f(1, "z\n"))]);
    let paths: BTreeSet<MPath> = [b"a".to_vec()].into();
    let r = merge_trees(&b, &o, &t, Some(&paths), MAX, &BTreeSet::new()).unwrap();
    assert_eq!(
        r,
        tree(&[("a", f(1, "2\n")), ("n", f(1, "new\n")), ("z", f(1, "Z\n"))])
    );
}

#[test]
fn all_conflicts_reported_in_path_order() {
    let b = tree(&[("a", f(1, "b")), ("c", f(1, "b"))]);
    let o = tree(&[("a", f(1, "o")), ("c", f(1, "o")), ("e", f(1, "1"))]);
    let t = tree(&[("c", f(1, "t")), ("e", f(1, "2")), ("g", f(1, "ok"))]);
    let r = merge_trees(&b, &o, &t, None, MAX, &BTreeSet::new());
    assert_eq!(
        kinds(r),
        vec![
            ("a".to_string(), "modify_delete"),
            ("c".to_string(), "content"),
            ("e".to_string(), "add_add"),
        ]
    );
}
