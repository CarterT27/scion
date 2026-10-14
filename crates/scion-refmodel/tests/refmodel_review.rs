//! Review additions: rebase invariants on random nested trees (SPEC 8.2, 18.3), validity of
//! clean merge results, the literal structural-rule reading, and extra content-merge edges
//! (SPEC 8.3, 19.4; line-diff3-v1 section 6).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use scion_refmodel::diff::{merge3, merge3_full};
use scion_refmodel::{
    MConflict, MDelta, MFile, MPath, MRebaseError, MRepo, MTree, apply_delta, file_diff,
    land_sequential, merge_trees,
};
use std::collections::BTreeSet;

const MAX: u64 = 10_485_760;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Default-run count (PLAN 3.5): `SCION_PROPTEST_CASES` or `default`.
fn cases(default: u64) -> u64 {
    std::env::var("SCION_PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Spec-sized count for the `#[ignore] full_*` variant: 10^6 under `SCION_FULL`.
fn full_cases() -> u64 {
    if std::env::var("SCION_FULL").is_ok() {
        1_000_000
    } else {
        cases(1_000_000)
    }
}

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

fn random_path(r: &mut Rng) -> Vec<u8> {
    let names: [&[u8]; 3] = [b"a", b"b", b"c"];
    let depth = 1 + r.below(3);
    let mut p = Vec::new();
    for i in 0..depth {
        if i > 0 {
            p.push(b'/');
        }
        p.extend_from_slice(names[r.below(3) as usize]);
    }
    p
}

fn random_file(r: &mut Rng) -> MFile {
    const TEXTS: [&str; 5] = ["1\n2\n3\n", "1\nX\n3\n", "1\n2\nY\n", "Z\n", "1\n2\n3\n4\n"];
    MFile {
        mode: 1 + r.below(2) as u8 + if r.below(8) == 0 { 1 } else { 0 },
        content: TEXTS[r.below(TEXTS.len() as u64) as usize]
            .as_bytes()
            .to_vec(),
    }
}

/// Insert `p` and drop every file that clashes with it.
fn insert_valid(t: &mut MTree, p: Vec<u8>, f: MFile) {
    let mut prefix = p.clone();
    prefix.push(b'/');
    t.retain(|k, _| {
        let mut kp = k.clone();
        kp.push(b'/');
        !(k.starts_with(&prefix) || p.starts_with(&kp))
    });
    t.insert(p, f);
}

fn random_tree(r: &mut Rng) -> MTree {
    let mut t = MTree::new();
    for _ in 0..r.below(8) {
        let p = random_path(r);
        let f = random_file(r);
        insert_valid(&mut t, p, f);
    }
    t
}

fn random_child(r: &mut Rng, t: &MTree) -> MTree {
    let mut u = t.clone();
    for _ in 0..r.below(4) {
        if r.below(3) == 0 && !u.is_empty() {
            let k = u
                .keys()
                .nth(r.below(u.len() as u64) as usize)
                .cloned()
                .unwrap();
            u.remove(&k);
        } else {
            let p = random_path(r);
            let f = random_file(r);
            insert_valid(&mut u, p, f);
        }
    }
    u
}

/// No file at a proper ancestor of another file.
fn valid_tree(t: &MTree) -> bool {
    t.keys().all(|k| {
        let mut kp = k.clone();
        kp.push(b'/');
        !t.keys().any(|o| o.starts_with(&kp))
    })
}

fn delta_paths(d: &[MDelta]) -> BTreeSet<MPath> {
    d.iter()
        .map(|e| match e {
            MDelta::Put(p, _) => p.clone(),
            MDelta::Delete(p) => p.clone(),
        })
        .collect()
}

fn run_rebase_invariants(n: u64, seed: u64) {
    let mut r = Rng(seed);
    let (mut clean, mut empty, mut conflict) = (0u64, 0u64, 0u64);
    for i in 0..n {
        let b = random_tree(&mut r);
        let o = random_child(&mut r, &b);
        let t = random_child(&mut r, &b);
        let mut repo = MRepo::new();
        let p = repo.commit(&[], b.clone());
        let x = repo.commit(&[p], o.clone());
        let c = repo.commit(&[p], t.clone());
        let paths = delta_paths(&file_diff(&b, &t));
        let first = repo.rebase(c, x, MAX);
        // Determinism: the same (C, X) gives the same outcome and tree.
        let second = repo.rebase(c, x, MAX);
        match (&first, &second) {
            (Ok(a), Ok(z)) => assert_eq!(repo.tree(*a), repo.tree(*z), "case {i}"),
            (a, z) => assert_eq!(a, z, "case {i}"),
        }
        match first {
            Ok(res) => {
                clean += 1;
                let rt = repo.tree(res).clone();
                assert_eq!(repo.parents(res), vec![x], "case {i}");
                assert!(valid_tree(&rt), "case {i}: invalid result {rt:?}");
                // Only delta paths differ from O, and the recomputed delta is a valid delta
                // against O that rebuilds the result.
                let d = file_diff(&o, &rt);
                assert!(!d.is_empty(), "case {i}");
                assert!(delta_paths(&d).is_subset(&paths), "case {i}");
                assert_eq!(apply_delta(&o, &d), Ok(rt.clone()), "case {i}");
                // Per-path table rows (SPEC 19.4) on the delta paths.
                for q in &paths {
                    let (bq, oq, tq) = (b.get(q), o.get(q), t.get(q));
                    if oq == bq {
                        assert_eq!(rt.get(q), tq, "case {i}: o == b at {q:?}");
                    } else if tq == bq || oq == tq {
                        assert_eq!(rt.get(q), oq, "case {i}: t == b or o == t at {q:?}");
                    }
                }
            }
            Err(MRebaseError::Empty) => {
                empty += 1;
                let m = merge_trees(&b, &o, &t, Some(&paths), MAX, &BTreeSet::new());
                assert_eq!(m, Ok(o.clone()), "case {i}");
            }
            Err(MRebaseError::Conflict(cs)) => {
                conflict += 1;
                assert!(!cs.is_empty(), "case {i}");
                let cp: Vec<&MPath> = cs.iter().map(|c| &c.path).collect();
                let mut sorted = cp.clone();
                sorted.sort();
                sorted.dedup();
                assert_eq!(cp, sorted, "case {i}: conflicts not ascending/unique");
                assert!(cs.iter().all(|c| paths.contains(&c.path)), "case {i}");
            }
        }
    }
    // The generator exercises every outcome.
    assert!(
        clean > 0 && empty > 0 && conflict > 0,
        "{clean} {empty} {conflict}"
    );
}

#[test]
fn prop_rebase_invariants() {
    run_rebase_invariants(cases(3_000), 0x5c10_0107);
}

#[test]
#[ignore]
fn full_prop_rebase_invariants() {
    run_rebase_invariants(full_cases(), 0x5c10_0108);
}

#[test]
fn prop_clean_merges_are_valid_trees() {
    // Merge(a, b) form (paths = None) over nested trees: a clean result never has a file at an
    // ancestor of another file, and every unexamined path keeps O.
    let mut r = Rng(0x5c10_0109);
    for i in 0..cases(3_000) {
        let b = random_tree(&mut r);
        let o = random_child(&mut r, &b);
        let t = random_child(&mut r, &b);
        if let Ok(m) = merge_trees(&b, &o, &t, None, MAX, &BTreeSet::new()) {
            assert!(valid_tree(&m), "case {i}: {b:?} {o:?} {t:?} -> {m:?}");
            for (k, v) in &o {
                if b.get(k) == Some(v) && t.get(k) == Some(v) {
                    assert_eq!(m.get(k), Some(v), "case {i}");
                }
            }
        }
    }
}

/// The structural rules follow the SPEC 8.3 text and the SPEC 18.3 order literally: they look at
/// O alone and win over the table rows. A file/directory transition rebased onto a target other
/// than its parent therefore conflicts even when the target never touched the path. This pins
/// the reading flagged for an orchestrator ruling; change it together with production.
#[test]
fn structural_rules_are_read_literally() {
    // directory -> file: B = {d/x}, T = {d}, O = B + {z}.
    let mut repo = MRepo::new();
    let p = repo.commit(&[], tree(&[("d/x", f(1, "x"))]));
    let x = repo.commit(&[p], tree(&[("d/x", f(1, "x")), ("z", f(1, "z"))]));
    let c = repo.commit(&[p], tree(&[("d", f(1, "file"))]));
    assert_eq!(
        repo.rebase(c, x, MAX),
        Err(MRebaseError::Conflict(vec![MConflict {
            path: b"d".to_vec(),
            kind: "file_directory"
        }]))
    );
    // Fast path is unaffected.
    assert_eq!(repo.rebase(c, p, MAX), Ok(c));
    // file -> directory: B = {a}, T = {a/b}, O = B + {z}: conflict at a/b (O has a file at the
    // ancestor a), while a itself resolves by o == b.
    let mut repo = MRepo::new();
    let p = repo.commit(&[], tree(&[("a", f(1, "a"))]));
    let x = repo.commit(&[p], tree(&[("a", f(1, "a")), ("z", f(1, "z"))]));
    let c = repo.commit(&[p], tree(&[("a/b", f(1, "b"))]));
    assert_eq!(
        repo.rebase(c, x, MAX),
        Err(MRebaseError::Conflict(vec![MConflict {
            path: b"a/b".to_vec(),
            kind: "file_directory"
        }]))
    );
    // Delete rule wins over o == t: both sides delete file p, O also created p/x.
    let b = tree(&[("p", f(1, "p"))]);
    let o = tree(&[("p/x", f(1, "x"))]);
    let t = MTree::new();
    let paths: BTreeSet<MPath> = [b"p".to_vec()].into();
    assert_eq!(
        merge_trees(&b, &o, &t, Some(&paths), MAX, &BTreeSet::new()),
        Err(vec![MConflict {
            path: b"p".to_vec(),
            kind: "file_directory"
        }])
    );
}

#[test]
fn structural_rules_do_not_fire_when_t_keeps_the_path() {
    // Merge form: O turns d into a file while T only edits elsewhere; no conflict.
    let b = tree(&[("d/x", f(1, "x")), ("k", f(1, "1\n"))]);
    let o = tree(&[("d", f(1, "file")), ("k", f(1, "1\n"))]);
    let t = tree(&[("d/x", f(1, "x")), ("k", f(1, "2\n"))]);
    assert_eq!(
        merge_trees(&b, &o, &t, None, MAX, &BTreeSet::new()),
        Ok(tree(&[("d", f(1, "file")), ("k", f(1, "2\n"))]))
    );
}

#[test]
fn content_merge_preserves_crlf_and_unterminated_lines_through_trees() {
    let b = tree(&[("p", f(1, "a\r\nb\r\nc"))]);
    let o = tree(&[("p", f(1, "A\r\nb\r\nc"))]);
    let t = tree(&[("p", f(1, "a\r\nb\r\nc\n"))]);
    assert_eq!(
        merge_trees(&b, &o, &t, None, MAX, &BTreeSet::new()),
        Ok(tree(&[("p", f(1, "A\r\nb\r\nc\n"))]))
    );
}

#[test]
fn mode_merge_with_unchanged_content_on_one_side() {
    // ours changes only the mode, theirs only the content: both apply.
    let b = tree(&[("p", f(1, "1\n2\n"))]);
    let o = tree(&[("p", f(2, "1\n2\n"))]);
    let t = tree(&[("p", f(1, "1\nTWO\n"))]);
    assert_eq!(
        merge_trees(&b, &o, &t, None, MAX, &BTreeSet::new()),
        Ok(tree(&[("p", f(2, "1\nTWO\n"))]))
    );
}

#[test]
fn insertion_inside_a_changed_range_touches_it() {
    // ours deletes lines 1..3; theirs inserts between base lines 1 and 2 (an empty range at 2,
    // inside [1, 3)): one conflict region (line-diff3-v1 section 6 step 3).
    let base = b"a\nb\nc\nd\n";
    let ours = b"a\nd\n";
    let theirs = b"a\nb\nN\nc\nd\n";
    assert_eq!(
        merge3_full(base, ours, theirs),
        Err(vec![((1, 3), (1, 1), (1, 4))])
    );
    // An insertion one line past the end of the deletion does not touch it.
    let theirs = b"a\nb\nc\nd\nN\n";
    assert_eq!(merge3(base, ours, theirs), Ok(b"a\nd\nN\n".to_vec()));
    // A deletion of a whole file against an edit of it is a content conflict.
    assert_eq!(merge3(b"x\n", b"", b"y\n"), Err(vec![(0, 1)]));
}

#[test]
fn land_sequential_matches_chained_rebases() {
    // Overlapping text edits that merge cleanly land in order, as chained rebases would.
    let base = tree(&[("f", f(1, "1\n2\n3\n4\n5\n6\n7\n"))]);
    let changes = vec![
        vec![MDelta::Put(b"f".to_vec(), f(1, "1\nA\n3\n4\n5\n6\n7\n"))],
        vec![MDelta::Put(b"f".to_vec(), f(1, "1\n2\n3\nB\n5\n6\n7\n"))],
        vec![MDelta::Put(b"f".to_vec(), f(1, "1\n2\n3\n4\n5\nC\n7\n"))],
        vec![MDelta::Put(b"g/h".to_vec(), f(2, "x"))],
    ];
    let mut repo = MRepo::new();
    let root = repo.commit(&[], base.clone());
    let mut onto = root;
    for d in &changes {
        let c = repo.commit(&[root], apply_delta(&base, d).unwrap());
        onto = repo.rebase(c, onto, MAX).unwrap();
    }
    let want = tree(&[("f", f(1, "1\nA\n3\nB\n5\nC\n7\n")), ("g/h", f(2, "x"))]);
    assert_eq!(repo.tree(onto), &want);
    assert_eq!(land_sequential(&base, &changes), Ok(want));
}
