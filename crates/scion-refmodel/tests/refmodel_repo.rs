//! MRepo commit graph, rebase, TREESAME log, blame and all-versions on hand DAGs; sequential
//! land; sparse materialization (SPEC 8.2, 8.4, 9.2, 11.2, 13.2, 18.5).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use scion_refmodel::{
    MConflict, MDelta, MFile, MRebaseError, MRepo, MTree, land_sequential, materialize,
};
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

#[test]
fn generation_and_skip_with_octopus() {
    let mut r = MRepo::new();
    let c0 = r.commit(&[], MTree::new());
    let c1 = r.commit(&[c0], MTree::new());
    let c2 = r.commit(&[c1], MTree::new());
    let c3 = r.commit(&[c1], MTree::new());
    let c4 = r.commit(&[c1], MTree::new());
    let c5 = r.commit(&[c2, c3, c4], MTree::new()); // octopus
    let c6 = r.commit(&[c5], MTree::new());
    let gens: Vec<u64> = (0..7).map(|c| r.generation(c)).collect();
    assert_eq!(gens, vec![1, 2, 3, 3, 3, 4, 5]);
    assert_eq!(r.skip(c0), Vec::<usize>::new());
    assert_eq!(r.skip(c1), vec![c0]);
    assert_eq!(r.skip(c5), vec![c2, c1]);
    assert_eq!(r.skip(c6), vec![c5, c2, c0]);
    // A deeper octopus side raises the generation through a non-first parent.
    let d1 = r.commit(&[c4], MTree::new());
    let d2 = r.commit(&[d1], MTree::new());
    let o = r.commit(&[c2, c3, d2], MTree::new());
    assert_eq!(r.generation(o), 6);
    assert_eq!(r.skip(o), vec![c2, c1]);
    assert!(r.is_ancestor(c4, c6));
    assert!(r.is_ancestor(c3, c6));
    assert!(r.is_ancestor(c6, c6));
    assert!(!r.is_ancestor(c6, c5));
    assert!(!r.is_ancestor(c2, c3));
    assert!(r.is_ancestor(d1, o));
}

#[test]
fn skip_on_long_chain() {
    let mut r = MRepo::new();
    let mut prev = r.commit(&[], MTree::new());
    for _ in 0..9 {
        prev = r.commit(&[prev], MTree::new());
    }
    assert_eq!(prev, 9);
    assert_eq!(r.generation(9), 10);
    assert_eq!(r.skip(9), vec![8, 7, 5, 1]);
    assert_eq!(r.skip(8), vec![7, 6, 4, 0]);
}

#[test]
fn criss_cross_gives_two_merge_bases() {
    let mut r = MRepo::new();
    let c0 = r.commit(&[], MTree::new());
    let c1 = r.commit(&[c0], MTree::new());
    let c2 = r.commit(&[c0], MTree::new());
    let c3 = r.commit(&[c1, c2], MTree::new());
    let c4 = r.commit(&[c2, c1], MTree::new());
    assert_eq!(r.merge_bases(c3, c4), vec![c1, c2]);
    // Simple cases.
    assert_eq!(r.merge_bases(c1, c2), vec![c0]);
    assert_eq!(r.merge_bases(c3, c1), vec![c1]);
    assert_eq!(r.merge_bases(c3, c3), vec![c3]);
    // Unrelated roots have no common ancestor.
    let other = r.commit(&[], MTree::new());
    assert!(r.merge_bases(c3, other).is_empty());
}

#[test]
fn rebase_fast_path_clean_empty_and_conflict() {
    let mut r = MRepo::new();
    let base = tree(&[("a", f(1, "1\n2\n3\n")), ("b", f(1, "b\n"))]);
    let p = r.commit(&[], base.clone());
    let mut tc = base.clone();
    tc.insert(b"a".to_vec(), f(1, "1\n2\nTHREE\n"));
    let c = r.commit(&[p], tc);
    // Fast path: onto == P returns C itself.
    assert_eq!(r.rebase(c, p, MAX), Ok(c));
    // Clean: onto changed an unrelated file and another line.
    let mut tx = base.clone();
    tx.insert(b"a".to_vec(), f(1, "ONE\n2\n3\n"));
    tx.insert(b"z".to_vec(), f(1, "z\n"));
    let x = r.commit(&[p], tx);
    let res = r.rebase(c, x, MAX).unwrap();
    assert_eq!(r.parents(res), vec![x]);
    assert_eq!(
        r.tree(res),
        &tree(&[
            ("a", f(1, "ONE\n2\nTHREE\n")),
            ("b", f(1, "b\n")),
            ("z", f(1, "z\n"))
        ])
    );
    // EMPTY_CHANGE: the change is already in the target.
    let mut ty = base.clone();
    ty.insert(b"a".to_vec(), f(1, "1\n2\nTHREE\n"));
    let y = r.commit(&[p], ty);
    assert_eq!(r.rebase(c, y, MAX), Err(MRebaseError::Empty));
    // Conflict.
    let mut tz = base.clone();
    tz.insert(b"a".to_vec(), f(1, "1\n2\ntri\n"));
    let z = r.commit(&[p], tz);
    assert_eq!(
        r.rebase(c, z, MAX),
        Err(MRebaseError::Conflict(vec![MConflict {
            path: b"a".to_vec(),
            kind: "content"
        }]))
    );
}

#[test]
fn rebase_examines_only_delta_paths() {
    // Onto adds a directory at a path the change never touches: no conflict.
    let mut r = MRepo::new();
    let base = tree(&[("a", f(1, "a\n"))]);
    let p = r.commit(&[], base.clone());
    let c = r.commit(&[p], tree(&[("a", f(1, "A\n"))]));
    let x = r.commit(&[p], tree(&[("a", f(1, "a\n")), ("d/e", f(1, "e\n"))]));
    let res = r.rebase(c, x, MAX).unwrap();
    assert_eq!(
        r.tree(res),
        &tree(&[("a", f(1, "A\n")), ("d/e", f(1, "e\n"))])
    );
}

/// c0 root; c1 adds f=v1; c2 (main) f=v2; c3 (side) f=v3 and adds g; c4 = merge [c2, c3]
/// keeping f=v2; c5 = merge [c2, c3] taking f=v3.
fn history_dag() -> (MRepo, [usize; 6]) {
    let mut r = MRepo::new();
    let c0 = r.commit(&[], MTree::new());
    let c1 = r.commit(&[c0], tree(&[("f", f(1, "v1\n"))]));
    let c2 = r.commit(&[c1], tree(&[("f", f(1, "v2\n"))]));
    let c3 = r.commit(&[c1], tree(&[("f", f(1, "v3\n")), ("g", f(1, "x\n"))]));
    let c4 = r.commit(&[c2, c3], tree(&[("f", f(1, "v2\n")), ("g", f(1, "x\n"))]));
    let c5 = r.commit(&[c2, c3], tree(&[("f", f(1, "v3\n")), ("g", f(1, "x\n"))]));
    (r, [c0, c1, c2, c3, c4, c5])
}

#[test]
fn treesame_log_merge_keeps_first_parent_version() {
    let (r, [_, c1, c2, c3, c4, c5]) = history_dag();
    // The merge kept the first parent's version: the node is reused, the side is not walked.
    assert_eq!(r.log_path(c4, b"f", false), vec![c2, c1]);
    assert_eq!(r.log_path(c4, b"f", true), vec![c2, c1]);
    // The merge took the second parent's version: a new node with both parents.
    assert_eq!(r.log_path(c5, b"f", false), vec![c5, c3, c2, c1]);
    assert_eq!(r.log_path(c5, b"f", true), vec![c5, c2, c1]);
    // g is absent in the first parent, so the merge creates a node whose parent is the side's.
    assert_eq!(r.log_path(c4, b"g", false), vec![c4, c3]);
    assert_eq!(r.log_path(c3, b"g", false), vec![c3]);
    // Absent path.
    assert!(r.log_path(c2, b"g", false).is_empty());
    assert!(r.log_path(c4, b"nope", false).is_empty());
}

#[test]
fn log_node_reuse_ignores_mode_and_tracks_directories() {
    let (mut r, [_, c1, c2, _, c4, _]) = history_dag();
    // A mode-only change keeps the blob id: the node is reused.
    let c6 = r.commit(&[c4], tree(&[("f", f(2, "v2\n")), ("g", f(1, "x\n"))]));
    assert_eq!(r.log_path(c6, b"f", false), vec![c2, c1]);
    // Directory history: d's tree changes at d1 and d3, not at d2.
    let d1 = r.commit(&[c6], tree(&[("d/x", f(1, "1"))]));
    let d2 = r.commit(&[d1], tree(&[("d/x", f(1, "1")), ("e", f(1, "e"))]));
    let d3 = r.commit(&[d2], tree(&[("d/x", f(2, "1")), ("e", f(1, "e"))]));
    assert_eq!(r.log_path(d3, b"d", false), vec![d3, d1]);
}

#[test]
fn blame_three_commits() {
    let mut r = MRepo::new();
    let c0 = r.commit(&[], MTree::new());
    let c1 = r.commit(&[c0], tree(&[("f", f(1, "a\nb\nc\n"))]));
    let c2 = r.commit(&[c1], tree(&[("f", f(1, "a\nB\nc\nd\n"))]));
    let c3 = r.commit(&[c2], tree(&[("f", f(1, "x\na\nB\nd\n"))]));
    assert_eq!(r.blame(c1, b"f"), vec![(0, 3, c1, 0)]);
    assert_eq!(
        r.blame(c2, b"f"),
        vec![(0, 1, c1, 0), (1, 1, c2, 1), (2, 1, c1, 2), (3, 1, c2, 3)]
    );
    assert_eq!(
        r.blame(c3, b"f"),
        vec![(0, 1, c3, 0), (1, 1, c1, 0), (2, 1, c2, 1), (3, 1, c2, 3)]
    );
    // Not a file.
    assert!(r.blame(c3, b"nope").is_empty());
    assert!(r.blame(c0, b"f").is_empty());
    // An unchanged descendant reuses the node, hence the blame.
    let c4 = r.commit(
        &[c3],
        tree(&[("f", f(1, "x\na\nB\nd\n")), ("o", f(1, "o"))]),
    );
    assert_eq!(r.blame(c4, b"f"), r.blame(c3, b"f"));
}

#[test]
fn blame_merge_uses_subsequent_parents() {
    let mut r = MRepo::new();
    let c0 = r.commit(&[], MTree::new());
    let c1 = r.commit(&[c0], tree(&[("f", f(1, "a\n"))]));
    let c2 = r.commit(&[c1], tree(&[("f", f(1, "a\nb\n"))]));
    let c3 = r.commit(&[c1], tree(&[("f", f(1, "c\na\n"))]));
    let c4 = r.commit(&[c2, c3], tree(&[("f", f(1, "c\na\nb\n"))]));
    assert_eq!(
        r.blame(c4, b"f"),
        vec![(0, 1, c3, 0), (1, 1, c1, 0), (2, 1, c2, 1)]
    );
}

#[test]
fn blame_treats_redacted_content_as_empty() {
    let mut r = MRepo::new();
    let c0 = r.commit(&[], MTree::new());
    let c1 = r.commit(&[c0], tree(&[("f", f(1, "secret\nb\n"))]));
    let c2 = r.commit(&[c1], tree(&[("f", f(1, "a\nb\n"))]));
    assert_eq!(r.blame(c2, b"f"), vec![(0, 1, c2, 0), (1, 1, c1, 1)]);
    let red: BTreeSet<Vec<u8>> = [b"secret\nb\n".to_vec()].into();
    assert_eq!(r.blame_redacted(c2, b"f", &red), vec![(0, 2, c2, 0)]);
    assert!(r.blame_redacted(c1, b"f", &red).is_empty());
}

#[test]
fn all_versions_includes_deleted_and_recreated_history() {
    let mut r = MRepo::new();
    let c0 = r.commit(&[], MTree::new());
    let c1 = r.commit(&[c0], tree(&[("f", f(1, "v1"))]));
    let c2 = r.commit(&[c1], MTree::new());
    let c3 = r.commit(&[c2], tree(&[("f", f(1, "v3"))]));
    let c4 = r.commit(&[c3], tree(&[("f", f(2, "v1"))]));
    let side = r.commit(&[c1], tree(&[("f", f(1, "side"))]));
    let m = r.commit(&[c4, side], tree(&[("f", f(2, "v1"))]));
    assert_eq!(
        r.all_versions(c4, b"f"),
        vec![b"v1".to_vec(), b"v3".to_vec()]
    );
    assert_eq!(
        r.all_versions(m, b"f"),
        vec![b"v1".to_vec(), b"side".to_vec(), b"v3".to_vec()]
    );
    assert!(r.all_versions(c0, b"f").is_empty());
}

fn put(p: &str, c: &str) -> MDelta {
    MDelta::Put(p.as_bytes().to_vec(), f(1, c))
}

#[test]
fn land_sequential_applies_in_order() {
    let base = tree(&[("a", f(1, "1\n2\n3\n4\n5\n")), ("b", f(1, "b\n"))]);
    let changes = vec![
        vec![put("a", "X\n2\n3\n4\n5\n")],
        vec![put("a", "1\n2\n3\n4\nY\n")],
        vec![put("c", "c\n")],
        vec![MDelta::Delete(b"b".to_vec())],
    ];
    assert_eq!(
        land_sequential(&base, &changes),
        Ok(tree(&[("a", f(1, "X\n2\n3\n4\nY\n")), ("c", f(1, "c\n"))]))
    );
    assert_eq!(land_sequential(&base, &[]), Ok(base.clone()));
}

#[test]
fn land_sequential_failures() {
    let base = tree(&[("a", f(1, "1\n2\n3\n"))]);
    // A true conflict at the second change.
    let r = land_sequential(
        &base,
        &[vec![put("a", "1\nX\n3\n")], vec![put("a", "1\nY\n3\n")]],
    );
    assert_eq!(
        r,
        Err((
            1,
            vec![MConflict {
                path: b"a".to_vec(),
                kind: "content"
            }]
        ))
    );
    // The same change twice: the second is EMPTY_CHANGE.
    let r = land_sequential(&base, &[vec![put("b", "b")], vec![put("b", "b")]]);
    assert_eq!(r, Err((1, vec![])));
    // An invalid delta against the base.
    let r = land_sequential(&base, &[vec![MDelta::Delete(b"zz".to_vec())]]);
    assert_eq!(r, Err((0, vec![])));
}

#[test]
fn materialize_nested_sparse_prefixes() {
    let t = tree(&[
        ("a/b/c/f1", f(1, "1")),
        ("a/b/f2", f(2, "2")),
        ("a/f3", f(1, "3")),
        ("ab/f4", f(1, "4")),
        ("a/bc", f(3, "target")),
        ("x/y", f(1, "y")),
        ("top", f(1, "t")),
    ]);
    let s = |ps: &[&str]| -> Vec<Vec<u8>> { ps.iter().map(|p| p.as_bytes().to_vec()).collect() };
    let m = materialize(&t, &s(&["a/b", "a/b/c", "x/y"]));
    assert_eq!(
        m.keys().cloned().collect::<Vec<_>>(),
        s(&["a/b/c/f1", "a/b/f2", "x/y"])
    );
    // Contents and modes are carried.
    assert_eq!(m.get(b"a/b/f2".as_slice()), Some(&f(2, "2")));
    // A nested prefix alone; a file prefix; the whole tree; nothing.
    assert_eq!(
        materialize(&t, &s(&["a/b/c"]))
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        s(&["a/b/c/f1"])
    );
    assert_eq!(
        materialize(&t, &s(&["a/bc"]))
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        s(&["a/bc"])
    );
    assert_eq!(materialize(&t, &s(&[""])), t);
    assert_eq!(materialize(&t, &s(&["a", ""])), t);
    assert!(materialize(&t, &[]).is_empty());
    assert!(materialize(&t, &s(&["nope"])).is_empty());
}
