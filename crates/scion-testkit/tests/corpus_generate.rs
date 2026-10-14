//! SPEC 19.0 Corpus: the seeded generator is deterministic per seed and realizes the requested
//! shapes (octopus merges, multiple roots, criss-cross merges), detected structurally here with
//! an ancestry computation independent of the generator.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use scion_refmodel::MTree;
use scion_testkit::{CorpusSpec, GenRepo, generate};
use std::collections::BTreeSet;

fn spec(seed: u64) -> CorpusSpec {
    CorpusSpec {
        files: 120,
        depth: 4,
        fanout: 5,
        history_len: 150,
        merge_ratio: 0.3,
        octopus_ratio: 0.12,
        binary_ratio: 0.1,
        symlink_ratio: 0.1,
        seed,
    }
}

// ---- independent structural helpers ----

fn ancestors(r: &GenRepo, c: usize) -> BTreeSet<usize> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![c];
    while let Some(x) = stack.pop() {
        if seen.insert(x) {
            stack.extend(r.commits[x].parents.iter().copied());
        }
    }
    seen
}

/// Maximal common ancestors.
fn merge_bases(r: &GenRepo, a: usize, b: usize) -> Vec<usize> {
    let aa = ancestors(r, a);
    let ab = ancestors(r, b);
    let common: Vec<usize> = aa.intersection(&ab).copied().collect();
    common
        .iter()
        .copied()
        .filter(|&c| {
            !common
                .iter()
                .any(|&d| d != c && ancestors(r, d).contains(&c))
        })
        .collect()
}

/// A pair of commits with two or more merge bases (criss-cross), if any.
fn find_criss_cross(r: &GenRepo) -> Option<(usize, usize)> {
    let merges: Vec<usize> = (0..r.commits.len())
        .filter(|i| r.commits[*i].parents.len() >= 2)
        .collect();
    for (k, &a) in merges.iter().enumerate() {
        for &b in &merges[k + 1..] {
            if merge_bases(r, a, b).len() >= 2 {
                return Some((a, b));
            }
        }
    }
    None
}

fn valid_name(n: &[u8]) -> bool {
    !n.is_empty()
        && n.len() <= 255
        && !n.contains(&0)
        && !n.contains(&b'/')
        && n != b"."
        && n != b".."
        && !n.eq_ignore_ascii_case(b".scion")
}

fn check_tree(t: &MTree, max_depth: Option<usize>) {
    for (p, f) in t {
        assert!(p.len() <= 4096, "path too long");
        let comps: Vec<&[u8]> = p.split(|b| *b == b'/').collect();
        assert!(comps.iter().all(|c| valid_name(c)), "invalid path {p:?}");
        if let Some(d) = max_depth {
            assert!(
                comps.len() <= d,
                "path deeper than {d}: {}",
                String::from_utf8_lossy(p)
            );
        }
        assert!((1..=3).contains(&f.mode), "bad mode {}", f.mode);
        // No file is also a directory prefix of another path.
        let mut prefix = p.clone();
        prefix.push(b'/');
        let next = t.range(prefix.clone()..).next();
        assert!(
            !next.is_some_and(|(q, _)| q.starts_with(&prefix)),
            "file/dir conflict at {}",
            String::from_utf8_lossy(p)
        );
    }
}

fn lcs(a: &[&[u8]], b: &[&[u8]]) -> usize {
    let mut dp = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    dp[0][0]
}

// ---- tests ----

#[test]
fn deterministic_per_seed() {
    for seed in [0u64, 1, 42, u64::MAX] {
        let a = generate(&spec(seed));
        let b = generate(&spec(seed));
        assert_eq!(format!("{a:?}"), format!("{b:?}"), "seed {seed}");
    }
    let a = generate(&spec(1));
    let b = generate(&spec(2));
    assert_ne!(format!("{a:?}"), format!("{b:?}"));
}

#[test]
fn histories_are_topological_and_valid() {
    for seed in 0..4 {
        let s = spec(seed);
        let r = generate(&s);
        assert_eq!(r.commits.len(), s.history_len as usize);
        assert_eq!(r.commits[0].parents, Vec::<usize>::new());
        let mut change_ids = BTreeSet::new();
        for (i, c) in r.commits.iter().enumerate() {
            assert!(
                c.parents.iter().all(|p| *p < i),
                "commit {i} not topological"
            );
            let uniq: BTreeSet<_> = c.parents.iter().collect();
            assert_eq!(uniq.len(), c.parents.len(), "duplicate parent in {i}");
            if i > 0 {
                assert!(!c.parents.is_empty(), "generate() has a single root");
            }
            assert!(!c.author.is_empty() && !c.message.is_empty());
            change_ids.insert(c.change_id);
            check_tree(&c.tree, Some(s.depth as usize));
        }
        assert_eq!(change_ids.len(), r.commits.len(), "change ids are distinct");
        // Regular commits always change the tree.
        for (i, c) in r.commits.iter().enumerate() {
            if c.parents.len() == 1 {
                assert_ne!(c.tree, r.commits[c.parents[0]].tree, "commit {i} is empty");
            }
        }
    }
}

#[test]
fn realizes_octopus_merges_and_criss_cross() {
    for seed in 0..5 {
        let r = generate(&spec(seed));
        let octopus = r.commits.iter().filter(|c| c.parents.len() >= 3).count();
        let merges = r.commits.iter().filter(|c| c.parents.len() == 2).count();
        assert!(octopus >= 1, "seed {seed}: no octopus merge");
        assert!(merges >= 5, "seed {seed}: only {merges} merges");
        // Merge parents are never ancestors of one another.
        for (i, c) in r.commits.iter().enumerate() {
            for &p in &c.parents {
                for &q in &c.parents {
                    if p != q {
                        assert!(
                            !ancestors(&r, q).contains(&p),
                            "commit {i}: redundant parent"
                        );
                    }
                }
            }
        }
        let cc = find_criss_cross(&r);
        assert!(cc.is_some(), "seed {seed}: no criss-cross shape");
        let (a, b) = cc.unwrap_or_default();
        let mut expected = merge_bases(&r, a, b);
        expected.sort();
        assert_eq!(r.merge_bases(a, b), expected, "GenRepo::merge_bases agrees");
    }
}

#[test]
fn content_mix_follows_ratios() {
    let r = generate(&spec(3));
    let all: Vec<_> = r.commits.iter().flat_map(|c| c.tree.values()).collect();
    assert!(all.iter().any(|f| f.content.contains(&0)), "binary files");
    assert!(all.iter().any(|f| f.mode == 3), "symlinks");
    assert!(all.iter().any(|f| f.mode == 2), "executables");

    let plain = generate(&CorpusSpec {
        merge_ratio: 0.0,
        octopus_ratio: 0.0,
        binary_ratio: 0.0,
        symlink_ratio: 0.0,
        ..spec(3)
    });
    assert!(
        plain.commits.iter().all(|c| c.parents.len() <= 1),
        "no merges"
    );
    assert_eq!(plain.roots(), vec![0]);
    for c in &plain.commits {
        for f in c.tree.values() {
            assert!(!f.content.contains(&0));
            assert_ne!(f.mode, 3);
        }
    }
}

#[test]
fn file_count_and_shape_follow_spec() {
    for files in [10u64, 200, 1000] {
        let s = CorpusSpec {
            files,
            history_len: 40,
            ..spec(9)
        };
        let r = generate(&s);
        assert_eq!(
            r.commits[0].tree.len() as u64,
            files,
            "root holds `files` files"
        );
        let last = &r.commits[r.commits.len() - 1].tree;
        let n = last.len() as u64;
        assert!(
            n * 2 >= files && n <= files * 2 + 20,
            "final {n} vs {files}"
        );
    }
    // Directory names come from `fanout` choices per level.
    let s = CorpusSpec {
        fanout: 3,
        ..spec(4)
    };
    let r = generate(&s);
    for p in r.commits[0].tree.keys() {
        let comps: Vec<&[u8]> = p.split(|b| *b == b'/').collect();
        for d in &comps[..comps.len() - 1] {
            assert!(
                matches!(*d, b"d0" | b"d1" | b"d2"),
                "{:?}",
                String::from_utf8_lossy(d)
            );
        }
    }
}

#[test]
fn text_edits_are_line_level() {
    let r = generate(&spec(5));
    let mut checked = 0;
    for c in &r.commits {
        let [p] = c.parents[..] else { continue };
        let before = &r.commits[p].tree;
        for (path, f) in &c.tree {
            let Some(old) = before.get(path) else {
                continue;
            };
            let text = |b: &[u8]| !b.contains(&0);
            if old.mode == 3 || f.mode == 3 || !text(&old.content) || !text(&f.content) {
                continue;
            }
            if old.content == f.content {
                continue;
            }
            let a: Vec<&[u8]> = old.content.split_inclusive(|b| *b == b'\n').collect();
            let b: Vec<&[u8]> = f.content.split_inclusive(|b| *b == b'\n').collect();
            let common = lcs(&a, &b);
            let changed = (a.len() - common) + (b.len() - common);
            // At most 3 ops per commit, each 1-3 line operations (replace = 2 changes).
            assert!(changed <= 18, "{changed} line changes in one commit");
            assert!(common > 0 || a.len() <= 3, "edit rewrote the whole file");
            checked += 1;
        }
    }
    assert!(checked > 20, "only {checked} text edits checked");
}

#[test]
fn canonical_criss_cross_shape() {
    let r = GenRepo::criss_cross(&CorpusSpec {
        files: 30,
        ..spec(11)
    });
    assert_eq!(r.commits.len(), 7);
    let (a2, b2) = (5, 6);
    let mut bases = merge_bases(&r, a2, b2);
    bases.sort();
    assert_eq!(bases, vec![1, 2], "two merge bases a1 and b1");
    assert_eq!(r.merge_bases(a2, b2), vec![1, 2]);
    assert_eq!(r.commits[3].parents, vec![1, 2]);
    assert_eq!(r.commits[4].parents, vec![2, 1]);
    assert!(find_criss_cross(&r).is_some());
    for c in &r.commits {
        check_tree(&c.tree, None);
    }
}

#[test]
fn multiple_roots_shape() {
    for roots in [2usize, 3, 5] {
        let r = GenRepo::multi_root(
            &CorpusSpec {
                files: 40,
                ..spec(roots as u64)
            },
            roots,
        );
        let found: Vec<usize> = (0..r.commits.len())
            .filter(|i| r.commits[*i].parents.is_empty())
            .collect();
        assert_eq!(found.len(), roots);
        assert_eq!(r.roots(), found);
        let last = r.commits.len() - 1;
        assert_eq!(r.commits[last].parents.len(), roots, "joined by one merge");
        // Unrelated histories: no common ancestor between different roots' tips.
        let tips = &r.commits[last].parents;
        assert!(merge_bases(&r, tips[0], tips[1]).is_empty());
        for k in 0..roots {
            let prefix = format!("root{k}/").into_bytes();
            assert!(r.commits[last].tree.keys().any(|p| p.starts_with(&prefix)));
        }
        for c in &r.commits {
            check_tree(&c.tree, None);
        }
    }
}
