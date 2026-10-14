//! SPEC 18.2 apply_delta subcodes, SPEC 4.3.4 root changeset rules and the round-trip property
//! `file_diff(t, apply_delta(t, d)) == d` (SPEC 19.2).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use scion_refmodel::enc::{
    changeset_bytes, changeset_for, changeset_id, meta_bytes, meta_id, tree_bytes_for_dir, tree_id,
};
use scion_refmodel::{MDelta, MFile, MMeta, MTree, apply_delta, file_diff};

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

fn put(p: &str, f: MFile) -> MDelta {
    MDelta::Put(p.as_bytes().to_vec(), f)
}

fn del(p: &str) -> MDelta {
    MDelta::Delete(p.as_bytes().to_vec())
}

#[test]
fn subcode_delete_absent() {
    let t = tree(&[("a", f(1, "x"))]);
    assert_eq!(apply_delta(&t, &[del("b")]), Err("delete_absent"));
    // Below a file: the directory does not exist, so the path is absent.
    assert_eq!(apply_delta(&t, &[del("a/b")]), Err("delete_absent"));
}

#[test]
fn subcode_delete_not_file() {
    let t = tree(&[("d/x", f(1, "x"))]);
    assert_eq!(apply_delta(&t, &[del("d")]), Err("delete_not_file"));
}

#[test]
fn subcode_noop_put() {
    let t = tree(&[("a", f(1, "x"))]);
    assert_eq!(apply_delta(&t, &[put("a", f(1, "x"))]), Err("noop_put"));
    // A mode change is not a no-op.
    assert_eq!(
        apply_delta(&t, &[put("a", f(2, "x"))]),
        Ok(tree(&[("a", f(2, "x"))]))
    );
}

#[test]
fn subcode_file_directory_both_directions() {
    // Put below a file that is not deleted.
    let t = tree(&[("a", f(1, "x"))]);
    assert_eq!(
        apply_delta(&t, &[put("a/b", f(1, "y"))]),
        Err("file_directory")
    );
    // Put at a directory that is not fully deleted.
    let t = tree(&[("d/x", f(1, "x")), ("d/y", f(1, "y"))]);
    assert_eq!(
        apply_delta(&t, &[put("d", f(1, "file"))]),
        Err("file_directory")
    );
    assert_eq!(
        apply_delta(&t, &[put("d", f(1, "file")), del("d/x")]),
        Err("file_directory")
    );
}

#[test]
fn shape_subcodes() {
    let t = MTree::new();
    assert_eq!(
        apply_delta(&t, &[put("b", f(1, "")), put("a", f(1, ""))]),
        Err("delta_unsorted")
    );
    assert_eq!(
        apply_delta(&t, &[put("a", f(1, "")), del("a")]),
        Err("delta_duplicate")
    );
    for bad in [
        "", ".", "..", "a/./b", "x/..", ".scion", "q/.SCION", "/a", "a/", "a//b", "a\0b",
    ] {
        assert_eq!(
            apply_delta(&t, &[put(bad, f(1, ""))]),
            Err("invalid_path"),
            "{bad:?}"
        );
    }
    assert_eq!(apply_delta(&t, &[put("a", f(4, ""))]), Err("invalid_mode"));
    assert_eq!(apply_delta(&t, &[put("a", f(0, ""))]), Err("invalid_mode"));
}

#[test]
fn deepest_directory_fails_first() {
    // Two failures: "a" (file_directory, found at the root) and "z/q/r" (delete_absent, found
    // in the deeper directory "z/q", which the 18.2 walk visits first).
    let t = tree(&[("a", f(1, "x"))]);
    assert_eq!(
        apply_delta(&t, &[put("a/b", f(1, "y")), del("z/q/r")]),
        Err("delete_absent")
    );
}

#[test]
fn transitions_with_required_deletes_succeed() {
    // file -> directory
    let t = tree(&[("a", f(1, "x")), ("k", f(1, "k"))]);
    let r = apply_delta(&t, &[del("a"), put("a/b", f(1, "y"))]).unwrap();
    assert_eq!(r, tree(&[("a/b", f(1, "y")), ("k", f(1, "k"))]));
    // directory -> file
    let t = tree(&[("d/x", f(1, "x")), ("d/e/y", f(1, "y"))]);
    let r = apply_delta(&t, &[put("d", f(2, "file")), del("d/e/y"), del("d/x")]).unwrap();
    assert_eq!(r, tree(&[("d", f(2, "file"))]));
}

#[test]
fn last_file_removes_directory_and_root_may_become_empty() {
    let t = tree(&[("d/e/x", f(1, "x")), ("top", f(1, "t"))]);
    let r = apply_delta(&t, &[del("d/e/x")]).unwrap();
    assert_eq!(r, tree(&[("top", f(1, "t"))]));
    assert_eq!(
        tree_bytes_for_dir(&r, b""),
        tree_bytes_for_dir(&tree(&[("top", f(1, "t"))]), b"")
    );
    let r = apply_delta(&r, &[del("top")]).unwrap();
    assert!(r.is_empty());
    assert_eq!(tree_bytes_for_dir(&r, b""), vec![1, 0, 0]);
}

fn root_meta() -> MMeta {
    // PLAN 6.5 bootstrap Meta.
    MMeta {
        author: b"scion <scion@localhost>".to_vec(),
        author_time: (0, 0),
        committer: None,
        committer_time: None,
        creator: b"service:scion".to_vec(),
        message: b"root".to_vec(),
        ext: vec![],
    }
}

#[test]
fn root_changeset_rules() {
    // The root changeset: no parents, empty root tree, so T1 is empty and the delta is empty.
    let empty = MTree::new();
    assert!(file_diff(&empty, &empty).is_empty());
    assert_eq!(apply_delta(&empty, &[]), Ok(MTree::new()));
    let meta = root_meta();
    let mut change_id = [0u8; 16];
    change_id.copy_from_slice(&blake3::hash(b"scion-root-v1").as_bytes()[..16]);
    let cs = changeset_for(vec![], &empty, &empty, meta_id(&meta), change_id);
    assert!(cs.parents.is_empty());
    assert!(cs.delta.is_empty());
    assert_eq!(cs.root_tree, tree_id(&empty));
    // Manual encoding: version, 0 parents, root tree, meta, change id, 0 delta entries, 0 ext.
    let mut want = vec![1, 0];
    want.extend_from_slice(&tree_id(&empty));
    want.extend_from_slice(&meta_id(&meta));
    want.extend_from_slice(&change_id);
    want.extend_from_slice(&[0, 0]);
    assert_eq!(changeset_bytes(&cs), want);
    assert_eq!(changeset_id(&cs)[..2], [1, 3]);
    // Meta bytes: version, author, time (0, 0), no committer, no committer time, creator,
    // message, no extensions.
    let mut m = vec![1, 23];
    m.extend_from_slice(b"scion <scion@localhost>");
    m.extend_from_slice(&[0, 0, 0, 0, 13]);
    m.extend_from_slice(b"service:scion");
    m.extend_from_slice(&[4]);
    m.extend_from_slice(b"root");
    m.push(0);
    assert_eq!(meta_bytes(&meta), m);
    // A child of the root: delta relative to the root's empty tree.
    let t = tree(&[("a/b", f(1, "hi"))]);
    let child = changeset_for(vec![changeset_id(&cs)], &empty, &t, meta_id(&meta), [7; 16]);
    assert_eq!(child.delta.len(), 1);
    assert_eq!(child.delta[0].0, b"a/b".to_vec());
    assert_eq!(
        child.delta[0].1,
        Some((1, scion_refmodel::enc::blob_id(b"hi"), 2))
    );
}

#[test]
fn meta_with_committer_and_extensions() {
    let m = MMeta {
        author: b"a".to_vec(),
        author_time: (-1, 60),
        committer: Some(b"c".to_vec()),
        committer_time: Some((300, -120)),
        creator: b"service:imp".to_vec(),
        message: b"".to_vec(),
        ext: vec![(2, vec![0xab; 20])],
    };
    let mut want = vec![
        1, 1, b'a', 1, 120, 1, 1, b'c', 1, 0xd8, 0x04, 0xef, 0x01, 11,
    ];
    want.extend_from_slice(b"service:imp");
    want.extend_from_slice(&[0, 1, 2, 20]);
    want.extend_from_slice(&[0xab; 20]);
    assert_eq!(meta_bytes(&m), want);
}

// ---- property: file_diff(t, apply_delta(t, d)) == d ----

/// SplitMix64, so the test needs no third-party crates.
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

fn cases(default: u64) -> u64 {
    if std::env::var("SCION_FULL").is_ok() {
        return 1_000_000;
    }
    std::env::var("SCION_PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn random_path(r: &mut Rng) -> Vec<u8> {
    let names: [&[u8]; 4] = [b"a", b"b", b"c", b"d"];
    let depth = 1 + r.below(3);
    let mut p = Vec::new();
    for i in 0..depth {
        if i > 0 {
            p.push(b'/');
        }
        p.extend_from_slice(names[r.below(4) as usize]);
    }
    p
}

fn random_file(r: &mut Rng) -> MFile {
    MFile {
        mode: 1 + r.below(3) as u8,
        content: vec![b'0' + r.below(3) as u8],
    }
}

/// Insert `p` and drop every file that clashes with it (a file at an ancestor or below it).
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
    for _ in 0..r.below(10) {
        let p = random_path(r);
        let f = random_file(r);
        insert_valid(&mut t, p, f);
    }
    t
}

/// A random valid target: random puts (with file/directory transitions) and deletes.
fn random_target(r: &mut Rng, t: &MTree) -> MTree {
    let mut u = t.clone();
    for _ in 0..r.below(6) {
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

fn run_roundtrip(n: u64, seed: u64) {
    let mut r = Rng(seed);
    for i in 0..n {
        let t = random_tree(&mut r);
        let target = random_target(&mut r, &t);
        let d = file_diff(&t, &target);
        let applied =
            apply_delta(&t, &d).unwrap_or_else(|e| panic!("case {i}: {e} on {t:?} {d:?}"));
        assert_eq!(applied, target, "case {i}");
        assert_eq!(file_diff(&t, &applied), d, "case {i}");
        // Applying the result again is a no-op put / absent delete unless d is empty.
        if !d.is_empty() {
            assert!(apply_delta(&applied, &d).is_err(), "case {i}");
        }
        // The tree id is determined by the flat tree.
        assert_eq!(tree_id(&applied), tree_id(&target));
    }
}

#[test]
fn prop_file_diff_inverts_apply_delta() {
    run_roundtrip(cases(5_000), 0x5c10_0007);
}

#[test]
#[ignore]
fn full_prop_file_diff_inverts_apply_delta() {
    run_roundtrip(cases(1_000_000), 0x5c10_0008);
}

/// Independent characterization of validity: every delete names a file of `t`, every put
/// differs from `t`, and `t - deletes + puts` has no file at an ancestor of another file.
fn naive_valid(t: &MTree, d: &[MDelta]) -> Option<MTree> {
    let mut r = t.clone();
    for e in d {
        match e {
            MDelta::Delete(p) => {
                r.remove(p)?;
            }
            MDelta::Put(p, f) => {
                if t.get(p) == Some(f) {
                    return None;
                }
                r.insert(p.clone(), f.clone());
            }
        }
    }
    for k in r.keys() {
        let mut kp = k.clone();
        kp.push(b'/');
        if r.keys().any(|o| o.starts_with(&kp)) {
            return None;
        }
    }
    Some(r)
}

fn run_random_deltas(n: u64, seed: u64) {
    let mut r = Rng(seed);
    for i in 0..n {
        let t = random_tree(&mut r);
        let mut entries: std::collections::BTreeMap<Vec<u8>, Option<MFile>> = Default::default();
        for _ in 0..1 + r.below(5) {
            let p = if r.below(2) == 0 && !t.is_empty() {
                t.keys()
                    .nth(r.below(t.len() as u64) as usize)
                    .cloned()
                    .unwrap()
            } else {
                random_path(&mut r)
            };
            let op = if r.below(2) == 0 {
                None
            } else {
                Some(random_file(&mut r))
            };
            entries.insert(p, op);
        }
        let d: Vec<MDelta> = entries
            .into_iter()
            .map(|(p, op)| match op {
                Some(f) => MDelta::Put(p, f),
                None => MDelta::Delete(p),
            })
            .collect();
        match (apply_delta(&t, &d), naive_valid(&t, &d)) {
            (Ok(a), Some(b)) => {
                assert_eq!(a, b, "case {i}");
                assert_eq!(file_diff(&t, &a), d, "case {i}");
            }
            (Err(_), None) => {}
            (a, b) => panic!("case {i}: t={t:?} d={d:?} model={a:?} naive={b:?}"),
        }
    }
}

#[test]
fn prop_random_deltas_valid_iff_naive_valid() {
    run_random_deltas(cases(5_000), 0x5c10_0009);
}

#[test]
#[ignore]
fn full_prop_random_deltas_valid_iff_naive_valid() {
    run_random_deltas(cases(1_000_000), 0x5c10_000a);
}
