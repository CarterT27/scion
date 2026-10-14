//! Self-consistency properties of the model's Myers, diff3 and tree merge (SPEC 8.3, 19.4).
//! Default counts follow PLAN 3.5; `full_*` variants run the spec-sized counts.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use scion_refmodel::diff::{merge3, merge3_full, myers, split_lines};
use scion_refmodel::{MFile, MTree, merge_trees};
use std::collections::BTreeSet;

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

const LINES: [&str; 6] = ["a\n", "b\n", "c\n", "a\r\n", "d", "\n"];

fn random_text(r: &mut Rng) -> Vec<u8> {
    let n = r.below(7);
    let mut out = Vec::new();
    for i in 0..n {
        let l = LINES[r.below(LINES.len() as u64) as usize];
        // An unterminated line only at the end.
        if !l.ends_with('\n') && i + 1 != n {
            out.extend_from_slice(b"e\n");
        } else {
            out.extend_from_slice(l.as_bytes());
        }
    }
    out
}

/// A line-level random edit of `base`.
fn edit(r: &mut Rng, base: &[u8]) -> Vec<u8> {
    let mut ls: Vec<Vec<u8>> = split_lines(base).iter().map(|l| l.to_vec()).collect();
    for _ in 0..r.below(3) {
        let pos = r.below(ls.len() as u64 + 1) as usize;
        match r.below(3) {
            0 if pos < ls.len() => {
                ls.remove(pos);
            }
            1 if pos < ls.len() => ls[pos] = b"X\n".to_vec(),
            _ => ls.insert(pos.min(ls.len()), b"N\n".to_vec()),
        }
    }
    ls.concat()
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

fn run_myers(n: u64, seed: u64) {
    let mut r = Rng(seed);
    for i in 0..n {
        let ab = random_text(&mut r);
        let bb = if r.below(2) == 0 {
            edit(&mut r, &ab)
        } else {
            random_text(&mut r)
        };
        let a = split_lines(&ab);
        let b = split_lines(&bb);
        let script = myers(&a, &b);
        // The script consumes both sequences in order and rebuilds b.
        let (mut x, mut y) = (0, 0);
        let mut rebuilt: Vec<u8> = Vec::new();
        let mut changes = 0;
        let mut prev = '=';
        for &(op, ai, bi) in &script {
            match op {
                '=' => {
                    assert_eq!((ai, bi), (x, y), "case {i}");
                    assert_eq!(a[ai], b[bi], "case {i}");
                    rebuilt.extend_from_slice(b[bi]);
                    x += 1;
                    y += 1;
                }
                '-' => {
                    assert_eq!(ai, x, "case {i}");
                    assert_ne!(prev, '+', "case {i}: delete after insert in a run");
                    x += 1;
                    changes += 1;
                }
                _ => {
                    assert_eq!(bi, y, "case {i}");
                    rebuilt.extend_from_slice(b[bi]);
                    y += 1;
                    changes += 1;
                }
            }
            prev = op;
        }
        assert_eq!((x, y), (a.len(), b.len()), "case {i}");
        assert_eq!(rebuilt, bb, "case {i}");
        // Minimal: D = N + M - 2 * LCS.
        assert_eq!(changes, a.len() + b.len() - 2 * lcs(&a, &b), "case {i}");
    }
}

#[test]
fn prop_myers_script_is_valid_and_minimal() {
    run_myers(cases(10_000), 1);
}

#[test]
#[ignore]
fn full_prop_myers_script_is_valid_and_minimal() {
    run_myers(cases(1_000_000), 2);
}

fn run_merge3(n: u64, seed: u64) {
    let mut r = Rng(seed);
    for i in 0..n {
        let b = random_text(&mut r);
        let o = edit(&mut r, &b);
        let t = edit(&mut r, &b);
        assert_eq!(merge3(&b, &b, &t), Ok(t.clone()), "case {i}");
        assert_eq!(merge3(&b, &o, &b), Ok(o.clone()), "case {i}");
        assert_eq!(merge3(&b, &o, &o), Ok(o.clone()), "case {i}");
        // Symmetric up to swapping the ours and theirs ranges.
        match (merge3_full(&b, &o, &t), merge3_full(&b, &t, &o)) {
            (Ok(x), Ok(y)) => assert_eq!(x, y, "case {i}"),
            (Err(x), Err(y)) => {
                let swapped: Vec<_> = y.into_iter().map(|(bs, ts, os)| (bs, os, ts)).collect();
                assert_eq!(x, swapped, "case {i}");
            }
            (x, y) => panic!("case {i}: asymmetric {x:?} vs {y:?}"),
        }
    }
}

#[test]
fn prop_merge3_basic_laws() {
    run_merge3(cases(10_000), 3);
}

#[test]
#[ignore]
fn full_prop_merge3_basic_laws() {
    run_merge3(cases(1_000_000), 4);
}

#[test]
fn prop_disjoint_hunks_both_apply() {
    // Edits at lines separated by at least one unchanged line merge cleanly with both applied.
    let mut r = Rng(5);
    for i in 0..cases(10_000) {
        let n = 3 + r.below(8) as usize;
        let base: Vec<String> = (0..n).map(|k| format!("l{k}\n")).collect();
        let lo = r.below(n as u64 - 2) as usize;
        let hi = lo + 2 + r.below((n - lo - 2) as u64) as usize;
        let mut o = base.clone();
        o[lo] = "OURS\n".into();
        let mut t = base.clone();
        t[hi] = "THEIRS\n".into();
        let mut want = base.clone();
        want[lo] = "OURS\n".into();
        want[hi] = "THEIRS\n".into();
        assert_eq!(
            merge3(
                base.concat().as_bytes(),
                o.concat().as_bytes(),
                t.concat().as_bytes()
            ),
            Ok(want.concat().into_bytes()),
            "case {i}"
        );
        // Adjacent differing edits conflict.
        if lo + 1 < n {
            let mut t2 = base.clone();
            t2[lo + 1] = "THEIRS\n".into();
            assert!(
                merge3(
                    base.concat().as_bytes(),
                    o.concat().as_bytes(),
                    t2.concat().as_bytes()
                )
                .is_err(),
                "case {i}"
            );
        }
    }
}

fn random_tree(r: &mut Rng) -> MTree {
    let mut t = MTree::new();
    for _ in 0..r.below(6) {
        let p = vec![b'a' + r.below(5) as u8];
        let content = random_text(r);
        t.insert(
            p,
            MFile {
                mode: 1 + r.below(2) as u8,
                content,
            },
        );
    }
    t
}

fn run_tree_laws(n: u64, seed: u64) {
    let mut r = Rng(seed);
    let none = BTreeSet::new();
    for i in 0..n {
        let b = random_tree(&mut r);
        let o = random_tree(&mut r);
        let t = random_tree(&mut r);
        assert_eq!(
            merge_trees(&b, &b, &t, None, 1 << 20, &none),
            Ok(t.clone()),
            "case {i}"
        );
        assert_eq!(
            merge_trees(&b, &o, &b, None, 1 << 20, &none),
            Ok(o.clone()),
            "case {i}"
        );
        assert_eq!(
            merge_trees(&b, &o, &o, None, 1 << 20, &none),
            Ok(o.clone()),
            "case {i}"
        );
        // Clean merges are symmetric for flat trees (no directories, so no structural rules).
        let x = merge_trees(&b, &o, &t, None, 1 << 20, &none);
        let y = merge_trees(&b, &t, &o, None, 1 << 20, &none);
        match (x, y) {
            (Ok(x), Ok(y)) => assert_eq!(x, y, "case {i}"),
            (Err(x), Err(y)) => assert_eq!(x, y, "case {i}"),
            (x, y) => panic!("case {i}: {x:?} vs {y:?}"),
        }
    }
}

#[test]
fn prop_merge_trees_laws() {
    run_tree_laws(cases(5_000), 6);
}

#[test]
#[ignore]
fn full_prop_merge_trees_laws() {
    run_tree_laws(cases(1_000_000), 7);
}
