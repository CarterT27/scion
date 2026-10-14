//! SPEC 19.18 equal-depth generated repositories: the shared-subtree repo has the same depth at
//! 10^3, 10^5 and 10^7 files, and its distinct structural subtree count is O(depth x fanout).
//! Everything here is computed structurally from the DAG (no scion-object encoding).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use scion_refmodel::MFile;
use scion_testkit::{SharedSubtreeRepo, shared_subtree_repo};
use std::collections::{BTreeMap, BTreeSet};

const DEPTH: u32 = 8;

/// Structural key of every node, assigned bottom-up by an independent hash-consing pass:
/// two nodes get the same key iff their subtrees are structurally identical.
fn hash_cons(r: &SharedSubtreeRepo) -> Vec<usize> {
    type Key = (Vec<(Vec<u8>, usize)>, Vec<(Vec<u8>, MFile)>);
    let dirs = r.structural_dirs();
    let mut keys: Vec<usize> = Vec::with_capacity(dirs.len());
    let mut table: BTreeMap<String, usize> = BTreeMap::new();
    for (subdirs, files) in dirs {
        // Children precede parents, so child keys are known.
        let mut k: Key = (Vec::new(), files.clone());
        for (name, child) in subdirs {
            assert!(*child < keys.len(), "child after parent");
            k.0.push((name.clone(), keys[*child]));
        }
        k.0.sort();
        k.1.sort_by(|a, b| a.0.cmp(&b.0));
        let repr = format!("{k:?}");
        let next = table.len();
        keys.push(*table.entry(repr).or_insert(next));
    }
    keys
}

/// (logical file count, set of file depths) of every node, by memoized DAG traversal.
fn counts(r: &SharedSubtreeRepo) -> Vec<(u64, BTreeSet<u32>)> {
    let dirs = r.structural_dirs();
    let mut out: Vec<(u64, BTreeSet<u32>)> = Vec::with_capacity(dirs.len());
    for (subdirs, files) in dirs {
        let mut n = files.len() as u64;
        let mut depths: BTreeSet<u32> = if files.is_empty() {
            BTreeSet::new()
        } else {
            BTreeSet::from([1])
        };
        for (_, child) in subdirs {
            let (cn, cd) = &out[*child];
            n += cn;
            depths.extend(cd.iter().map(|d| d + 1));
        }
        out.push((n, depths));
    }
    out
}

fn check(files: u64) -> SharedSubtreeRepo {
    let r = shared_subtree_repo(files, DEPTH);
    assert_eq!(r.depth, DEPTH);
    assert_eq!(r.files, files);
    let c = counts(&r);
    let (n, depths) = &c[r.root_node()];
    assert_eq!(*n, files, "logical file count");
    assert_eq!(
        depths,
        &BTreeSet::from([DEPTH]),
        "every file at depth {DEPTH}"
    );

    // Distinct structural subtrees, recomputed independently.
    let keys = hash_cons(&r);
    let distinct: BTreeSet<usize> = keys.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        r.distinct_trees(),
        "distinct_trees() is exact"
    );
    assert!(
        distinct.len() <= 2 * DEPTH as usize,
        "{} trees",
        distinct.len()
    );

    // O(depth x fanout) objects and bytes: distinct trees x entries per tree, plus blobs.
    let entries: usize = r
        .structural_dirs()
        .iter()
        .map(|(s, f)| s.len() + f.len())
        .sum();
    let bound = 2 * DEPTH as usize * r.fanout as usize;
    assert!(entries <= bound, "{entries} entries > {bound}");
    assert!(r.distinct_blobs() <= r.fanout as usize);
    assert!(r.fanout as u64 >= 2 || files <= 1);
    // The fanout is minimal for this depth.
    assert!((r.fanout as u64).pow(DEPTH) >= files);
    assert!(((r.fanout - 1) as u64).pow(DEPTH) < files);

    // The perturbable path is a file at full depth.
    let p = r.perturb_path_bytes();
    assert_eq!(p.split(|b| *b == b'/').count(), DEPTH as usize);
    r
}

#[test]
fn same_depth_at_three_sizes() {
    let small = check(1_000);
    let medium = check(100_000);
    let large = check(10_000_000);
    assert!(small.fanout < medium.fanout && medium.fanout < large.fanout);
    // Distinct structure stays O(depth): it does not grow with the file count.
    assert!(large.distinct_trees() <= 2 * DEPTH as usize);
    assert!(small.distinct_trees() <= 2 * DEPTH as usize);
}

#[test]
fn other_depths_and_ragged_sizes() {
    for (files, depth) in [
        (10_000_000u64, 4u32),
        (12_345, 3),
        (999_999, 6),
        (1, 5),
        (7, 1),
    ] {
        let r = shared_subtree_repo(files, depth);
        let c = counts(&r);
        let (n, depths) = &c[r.root_node()];
        assert_eq!(*n, files);
        assert_eq!(depths, &BTreeSet::from([depth]));
        assert!(r.distinct_trees() <= 2 * depth as usize);
    }
}

#[test]
fn small_repo_materializes_to_the_same_structure() {
    let r = shared_subtree_repo(1_000, DEPTH);
    let t = r.materialize();
    assert_eq!(t.len(), 1_000);
    for p in t.keys() {
        assert_eq!(p.split(|b| *b == b'/').count(), DEPTH as usize);
    }
    assert!(t.contains_key(&r.perturb_path_bytes()));
    // Paths are distinct and valid names (no empty components).
    for p in t.keys() {
        assert!(
            p.split(|b| *b == b'/')
                .all(|c| !c.is_empty() && c.len() <= 255)
        );
    }
    // Leaf contents repeat across leaf directories: exactly `fanout` distinct blobs at most.
    let blobs: BTreeSet<&Vec<u8>> = t.values().map(|f| &f.content).collect();
    assert_eq!(blobs.len(), r.distinct_blobs());
}

#[test]
fn deterministic() {
    let a = shared_subtree_repo(100_000, DEPTH);
    let b = shared_subtree_repo(100_000, DEPTH);
    assert_eq!(a.structural_dirs(), b.structural_dirs());
    assert_eq!(a.perturb_path_bytes(), b.perturb_path_bytes());
}
