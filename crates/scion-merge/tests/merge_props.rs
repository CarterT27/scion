//! SPEC 19.4 content-level merge properties and Myers optimality (P02).
//!
//! Default runs use `proptest_cases(10_000)` per property; the `full_*` variants run the
//! spec-sized 10^6 cases and only when `SCION_FULL=1` (PLAN 3.5).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod merge_support;

use merge_support::*;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use scion_core::test_support::{full_mode, proptest_cases};
use scion_merge::{Merge3, merge3, myers, render_markers, split_lines};

const DEFAULT_CASES: u32 = 10_000;
const FULL_CASES: u32 = 1_000_000;

fn run<S: Strategy>(cases: u32, strategy: S, test: impl Fn(S::Value) -> Result<(), TestCaseError>)
where
    S::Value: std::fmt::Debug,
{
    let mut runner = TestRunner::new(Config {
        cases,
        failure_persistence: None,
        max_local_rejects: u32::MAX,
        max_global_rejects: u32::MAX,
        ..Config::default()
    });
    if let Err(e) = runner.run(&strategy, test) {
        panic!("{e}");
    }
}

fn full_or_skip() -> Option<u32> {
    if full_mode() {
        Some(proptest_cases(FULL_CASES))
    } else {
        eprintln!("SKIP: set SCION_FULL=1 for the spec-sized run");
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Generators.

/// Arbitrary text over a small line alphabet, including CRLF, empty and unterminated pieces.
fn arb_text() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop::sample::select(vec!["a\n", "b\n", "c\n", "\n", "a\r\n", "b", "\r"]),
        0..12,
    )
    .prop_map(|v| v.concat().into_bytes())
}

/// A base of unique lines plus line-ending style.
#[derive(Clone, Debug)]
struct Base {
    n: usize,
    crlf: bool,
    unterminated_last: bool,
}

impl Base {
    fn eol(&self) -> &'static str {
        if self.crlf { "\r\n" } else { "\n" }
    }
    fn line(&self, i: usize) -> String {
        if self.unterminated_last && i + 1 == self.n {
            format!("L{i}")
        } else {
            format!("L{i}{}", self.eol())
        }
    }
    fn lines(&self, r: std::ops::Range<usize>) -> String {
        r.map(|i| self.line(i)).collect()
    }
    fn text(&self) -> Vec<u8> {
        self.lines(0..self.n).into_bytes()
    }
    fn repl(&self, tag: &str, k: usize) -> String {
        (0..k).map(|j| format!("{tag}{j}{}", self.eol())).collect()
    }
    /// Apply one edit: replace `[s, e)` by `k` unique `tag` lines.
    fn apply(&self, edits: &[(usize, usize, usize, &str)]) -> Vec<u8> {
        let mut out = String::new();
        let mut pos = 0;
        for &(s, e, k, tag) in edits {
            out.push_str(&self.lines(pos..s));
            out.push_str(&self.repl(tag, k));
            pos = e;
        }
        out.push_str(&self.lines(pos..self.n));
        out.into_bytes()
    }
}

/// A base of 1..20 unique lines.
fn arb_base() -> impl Strategy<Value = Base> {
    (1usize..20, any::<bool>(), any::<bool>()).prop_map(|(n, crlf, unterminated_last)| Base {
        n,
        crlf,
        unterminated_last,
    })
}

/// An insertion at `n` after an unterminated final line would merge into that line; avoid it.
fn fix_unterminated(mut base: Base, edits: &[(usize, usize)]) -> Base {
    if edits.iter().any(|&(s, _)| s == base.n) {
        base.unterminated_last = false;
    }
    base
}

// ---------------------------------------------------------------------------------------------
// Property bodies.

fn prop_trivial_sides((b, x): (Vec<u8>, Vec<u8>)) -> Result<(), TestCaseError> {
    // o == b => t
    prop_assert_eq!(merge3(&b, &b, &x), Merge3::Clean(x.clone()));
    // t == b => o
    prop_assert_eq!(merge3(&b, &x, &b), Merge3::Clean(x.clone()));
    // o == t => o
    prop_assert_eq!(merge3(&b, &x, &x), Merge3::Clean(x.clone()));
    Ok(())
}

fn arb_disjoint()
-> impl Strategy<Value = (Base, (usize, usize, usize), (usize, usize, usize), bool)> {
    arb_base()
        .prop_flat_map(|base| {
            let n = base.n;
            // first edit [s1, e1), second [s2, e2) with s2 >= e1 + 1.
            (Just(base), 0..=n)
                .prop_flat_map(move |(base, s1)| (Just(base), Just(s1), s1..=n))
                .prop_flat_map(move |(base, s1, e1)| {
                    (Just(base), Just(s1), Just(e1), (e1 + 1)..=(n + 1))
                })
                .prop_flat_map(move |(base, s1, e1, s2)| {
                    let s2 = s2.min(n);
                    (Just(base), Just((s1, e1)), Just(s2), s2..=n)
                })
                .prop_flat_map(|(base, r1, s2, e2)| {
                    (
                        Just(base),
                        Just(r1),
                        Just((s2, e2)),
                        0usize..3,
                        0usize..3,
                        any::<bool>(),
                    )
                })
        })
        .prop_filter_map(
            "non-trivial disjoint edits",
            |(base, (s1, e1), (s2, e2), k1, k2, swap)| {
                let ok = s2 > e1 && (e1 > s1 || k1 > 0) && (e2 > s2 || k2 > 0);
                ok.then_some((base, (s1, e1, k1), (s2, e2, k2), swap))
            },
        )
}

fn prop_disjoint_both_apply(
    (base, (s1, e1, k1), (s2, e2, k2), swap): (
        Base,
        (usize, usize, usize),
        (usize, usize, usize),
        bool,
    ),
) -> Result<(), TestCaseError> {
    let base = fix_unterminated(base, &[(s1, e1), (s2, e2)]);
    let (t1, t2) = if swap { ("T", "O") } else { ("O", "T") };
    let first = base.apply(&[(s1, e1, k1, t1)]);
    let second = base.apply(&[(s2, e2, k2, t2)]);
    let both = base.apply(&[(s1, e1, k1, t1), (s2, e2, k2, t2)]);
    let (ours, theirs) = if swap {
        (&second, &first)
    } else {
        (&first, &second)
    };
    prop_assert_eq!(merge3(&base.text(), ours, theirs), Merge3::Clean(both));
    Ok(())
}

/// Two non-empty ranges sharing at least one line.
fn arb_overlap() -> impl Strategy<Value = (Base, (usize, usize, usize), (usize, usize, usize))> {
    arb_base()
        .prop_flat_map(|base| {
            let n = base.n;
            (Just(base), 0..n, 0..n, 0..n, 0..n, 1usize..3, 0usize..3)
        })
        .prop_filter_map("overlapping", |(base, a, b, c, d, k1, k2)| {
            let (s1, e1) = (a.min(b), a.max(b) + 1);
            let (s2, e2) = (c.min(d), c.max(d) + 1);
            (s1.max(s2) < e1.min(e2)).then_some((base, (s1, e1, k1), (s2, e2, k2)))
        })
}

fn expect_single_conflict(
    base: &Base,
    (s1, e1, k1): (usize, usize, usize),
    (s2, e2, k2): (usize, usize, usize),
) -> Result<(), TestCaseError> {
    let ours = base.apply(&[(s1, e1, k1, "O")]);
    let theirs = base.apply(&[(s2, e2, k2, "T")]);
    let (rs, re) = (s1.min(s2), e1.max(e2));
    let want = ch(
        rs as u32..re as u32,
        rs as u32..(s1 + k1 + re - e1) as u32,
        rs as u32..(s2 + k2 + re - e2) as u32,
    );
    prop_assert_eq!(
        merge3(&base.text(), &ours, &theirs),
        Merge3::Conflict(vec![want])
    );
    // Symmetric.
    let swapped = ch(
        rs as u32..re as u32,
        rs as u32..(s2 + k2 + re - e2) as u32,
        rs as u32..(s1 + k1 + re - e1) as u32,
    );
    prop_assert_eq!(
        merge3(&base.text(), &theirs, &ours),
        Merge3::Conflict(vec![swapped])
    );
    let markers = render_markers(&base.text(), &ours, &theirs);
    prop_assert!(markers.windows(13).any(|w| w == b"<<<<<<< ours\n"));
    Ok(())
}

fn prop_overlap_conflicts(
    (base, o, t): (Base, (usize, usize, usize), (usize, usize, usize)),
) -> Result<(), TestCaseError> {
    let base = fix_unterminated(base, &[(o.0, o.1), (t.0, t.1)]);
    expect_single_conflict(&base, o, t)
}

/// Adjacent edits: zero unchanged lines between, either two non-empty ranges sharing a
/// boundary or an insertion at either end of the other side's non-empty range.
fn arb_adjacent() -> impl Strategy<Value = (Base, (usize, usize, usize), (usize, usize, usize))> {
    arb_base()
        .prop_flat_map(|base| {
            let n = base.n;
            (
                Just(base),
                0..=n,
                0..=n,
                0..=n,
                0usize..3,
                1usize..3,
                0usize..3,
            )
        })
        .prop_filter_map("adjacent", |(base, a, b, c, mode, k1, k2)| {
            let mut v = [a, b, c];
            v.sort_unstable();
            let [x, y, z] = v;
            match mode {
                // [x, y) and [y, z), both non-empty
                0 if x < y && y < z => Some((base, (x, y, k1), (y, z, k2))),
                // insertion at the start of [y, z)
                1 if y < z => Some((base, (y, y, k1), (y, z, k2))),
                // insertion at the end of [x, y)
                2 if x < y => Some((base, (y, y, k1), (x, y, k2))),
                _ => None,
            }
        })
}

fn prop_adjacent_conflicts(
    (base, o, t): (Base, (usize, usize, usize), (usize, usize, usize)),
) -> Result<(), TestCaseError> {
    let base = fix_unterminated(base, &[(o.0, o.1), (t.0, t.1)]);
    expect_single_conflict(&base, o, t)
}

fn prop_identical_apply_once(
    (base, (s1, e1, k1), (s2, e2, k2), extra): (
        Base,
        (usize, usize, usize),
        (usize, usize, usize),
        bool,
    ),
) -> Result<(), TestCaseError> {
    let base = fix_unterminated(base, &[(s1, e1), (s2, e2)]);
    // Both sides make the same first edit; ours optionally adds a disjoint second edit.
    let shared = base.apply(&[(s1, e1, k1, "S")]);
    let ours = if extra {
        base.apply(&[(s1, e1, k1, "S"), (s2, e2, k2, "O")])
    } else {
        shared.clone()
    };
    prop_assert_eq!(
        merge3(&base.text(), &ours, &shared),
        Merge3::Clean(ours.clone())
    );
    prop_assert_eq!(merge3(&base.text(), &shared, &ours), Merge3::Clean(ours));
    Ok(())
}

fn arb_triple() -> impl Strategy<Value = (Vec<u8>, Vec<u8>, Vec<u8>)> {
    (arb_text(), arb_text(), arb_text())
}

fn prop_symmetric_and_deterministic(
    (b, o, t): (Vec<u8>, Vec<u8>, Vec<u8>),
) -> Result<(), TestCaseError> {
    let fwd = merge3(&b, &o, &t);
    prop_assert_eq!(&fwd, &merge3(&b, &o, &t));
    let rev = merge3(&b, &t, &o);
    match (&fwd, rev) {
        (Merge3::Clean(x), Merge3::Clean(y)) => {
            prop_assert_eq!(x, &y);
            prop_assert_eq!(&render_markers(&b, &o, &t), x);
        }
        (Merge3::Conflict(x), Merge3::Conflict(y)) => {
            let swapped: Vec<_> = y
                .into_iter()
                .map(|h| ch(h.base, h.theirs, h.ours))
                .collect();
            prop_assert_eq!(x, &swapped);
            // Conflict hunks lie within the inputs.
            let (nb, no, nt) = (
                split_lines(&b).len() as u32,
                split_lines(&o).len() as u32,
                split_lines(&t).len() as u32,
            );
            for h in x {
                prop_assert!(h.base.start <= h.base.end && h.base.end <= nb);
                prop_assert!(h.ours.start <= h.ours.end && h.ours.end <= no);
                prop_assert!(h.theirs.start <= h.theirs.end && h.theirs.end <= nt);
            }
        }
        (x, y) => prop_assert!(false, "asymmetric outcome {:?} vs {:?}", x, y),
    }
    Ok(())
}

fn arb_small_lines() -> impl Strategy<Value = (Vec<&'static str>, Vec<&'static str>)> {
    let line = prop::sample::select(vec!["a\n", "b\n", "c\n"]);
    (
        prop::collection::vec(line.clone(), 0..14),
        prop::collection::vec(line, 0..14),
    )
}

fn prop_myers_optimal((a, b): (Vec<&'static str>, Vec<&'static str>)) -> Result<(), TestCaseError> {
    let al = lines(&a);
    let bl = lines(&b);
    let edits = myers(&al, &bl);
    check_script(&al, &bl, &edits);
    prop_assert_eq!(
        script_cost(&edits),
        al.len() + bl.len() - 2 * lcs_len(&al, &bl)
    );
    prop_assert_eq!(&edits, &myers(&al, &bl));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Default-sized runs.

#[test]
fn merge_o_eq_b_t_eq_b_o_eq_t() {
    run(
        proptest_cases(DEFAULT_CASES),
        (arb_text(), arb_text()),
        prop_trivial_sides,
    );
}

#[test]
fn merge_disjoint_hunks_both_apply() {
    run(
        proptest_cases(DEFAULT_CASES),
        arb_disjoint(),
        prop_disjoint_both_apply,
    );
}

#[test]
fn merge_overlapping_hunks_conflict() {
    run(
        proptest_cases(DEFAULT_CASES),
        arb_overlap(),
        prop_overlap_conflicts,
    );
}

#[test]
fn merge_adjacent_hunks_conflict() {
    run(
        proptest_cases(DEFAULT_CASES),
        arb_adjacent(),
        prop_adjacent_conflicts,
    );
}

#[test]
fn merge_identical_hunks_apply_once() {
    run(
        proptest_cases(DEFAULT_CASES),
        arb_disjoint(),
        prop_identical_apply_once,
    );
}

#[test]
fn merge_symmetric_and_deterministic() {
    run(
        proptest_cases(DEFAULT_CASES),
        arb_triple(),
        prop_symmetric_and_deterministic,
    );
}

#[test]
fn myers_edit_length_is_minimal() {
    run(
        proptest_cases(DEFAULT_CASES),
        arb_small_lines(),
        prop_myers_optimal,
    );
}

// ---------------------------------------------------------------------------------------------
// Spec-sized runs (SPEC 19.4: 10^6 cases), `scripts/check.sh --full`.

#[test]
#[ignore = "spec-sized; run with SCION_FULL=1 --include-ignored"]
fn full_merge_o_eq_b_t_eq_b_o_eq_t() {
    if let Some(n) = full_or_skip() {
        run(n, (arb_text(), arb_text()), prop_trivial_sides);
    }
}

#[test]
#[ignore = "spec-sized; run with SCION_FULL=1 --include-ignored"]
fn full_merge_disjoint_hunks_both_apply() {
    if let Some(n) = full_or_skip() {
        run(n, arb_disjoint(), prop_disjoint_both_apply);
    }
}

#[test]
#[ignore = "spec-sized; run with SCION_FULL=1 --include-ignored"]
fn full_merge_overlapping_hunks_conflict() {
    if let Some(n) = full_or_skip() {
        run(n, arb_overlap(), prop_overlap_conflicts);
    }
}

#[test]
#[ignore = "spec-sized; run with SCION_FULL=1 --include-ignored"]
fn full_merge_adjacent_hunks_conflict() {
    if let Some(n) = full_or_skip() {
        run(n, arb_adjacent(), prop_adjacent_conflicts);
    }
}

#[test]
#[ignore = "spec-sized; run with SCION_FULL=1 --include-ignored"]
fn full_merge_identical_hunks_apply_once() {
    if let Some(n) = full_or_skip() {
        run(n, arb_disjoint(), prop_identical_apply_once);
    }
}

#[test]
#[ignore = "spec-sized; run with SCION_FULL=1 --include-ignored"]
fn full_merge_symmetric_and_deterministic() {
    if let Some(n) = full_or_skip() {
        run(n, arb_triple(), prop_symmetric_and_deterministic);
    }
}

#[test]
#[ignore = "spec-sized; run with SCION_FULL=1 --include-ignored"]
fn full_myers_edit_length_is_minimal() {
    if let Some(n) = full_or_skip() {
        run(n, arb_small_lines(), prop_myers_optimal);
    }
}
