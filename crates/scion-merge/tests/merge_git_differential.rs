//! RECOMMENDED SPEC 19.4 differential: on clean text merges, compare `merge3` with
//! `git merge-file -p`. Prints `SKIP: git not found` and passes when git is absent.
//!
//! Disagreements are classified and documented in `tests/GIT_MERGE_FILE_DIFFERENCES.md`. The
//! curated corpus (disjoint, non-adjacent edits of unique lines) must agree exactly; the random
//! small-alphabet corpus only reports its disagreement rate, because git's xdiff uses different
//! diff tie-breaking than line-diff3-v1 (which is normative for Scion).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod merge_support;

use merge_support::*;
use scion_merge::{Merge3, merge3};
use std::path::Path;
use std::process::Command;

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `git merge-file -p ours base theirs`: `Some(bytes)` when git merged cleanly.
fn git_merge_file(dir: &Path, base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    let (b, o, t) = (dir.join("base"), dir.join("ours"), dir.join("theirs"));
    std::fs::write(&b, base).unwrap();
    std::fs::write(&o, ours).unwrap();
    std::fs::write(&t, theirs).unwrap();
    let out = Command::new("git")
        .args(["merge-file", "-p"])
        .arg(&o)
        .arg(&b)
        .arg(&t)
        .output()
        .unwrap();
    match out.status.code() {
        Some(0) => Some(out.stdout),
        Some(c) if c > 0 && c < 128 => None,
        other => panic!(
            "git merge-file failed: {other:?} {}",
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

#[derive(Default, Debug)]
struct Tally {
    compared: usize,
    agree: usize,
    git_conflicts: usize,
    different_bytes: usize,
}

fn compare(dir: &Path, tally: &mut Tally, b: &[u8], o: &[u8], t: &[u8]) -> Option<String> {
    let Merge3::Clean(ours) = merge3(b, o, t) else {
        return None;
    };
    tally.compared += 1;
    match git_merge_file(dir, b, o, t) {
        Some(g) if g == ours => {
            tally.agree += 1;
            None
        }
        Some(g) => {
            tally.different_bytes += 1;
            Some(format!(
                "different bytes: base={:?} ours={:?} theirs={:?} scion={:?} git={:?}",
                String::from_utf8_lossy(b),
                String::from_utf8_lossy(o),
                String::from_utf8_lossy(t),
                String::from_utf8_lossy(&ours),
                String::from_utf8_lossy(&g)
            ))
        }
        None => {
            tally.git_conflicts += 1;
            Some(format!(
                "git conflicts, scion clean: base={:?} ours={:?} theirs={:?} scion={:?}",
                String::from_utf8_lossy(b),
                String::from_utf8_lossy(o),
                String::from_utf8_lossy(t),
                String::from_utf8_lossy(&ours)
            ))
        }
    }
}

/// Disjoint, non-adjacent edits (at least `gap` unchanged lines between) of unique lines.
fn curated_case(rng: &mut SplitMix, gap: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let n = 4 + rng.below(16);
    let line = |i: usize| format!("line {i}\n");
    let base: String = (0..n).map(line).collect();
    // ours edits [s1, e1), theirs edits [s2, e2), e1 + gap <= s2.
    let s1 = rng.below(n / 2);
    let e1 = s1 + rng.below(2);
    let s2 = (e1 + gap + rng.below(3)).min(n);
    let e2 = (s2 + rng.below(2)).min(n);
    let k1 = if e1 == s1 {
        1 + rng.below(2)
    } else {
        rng.below(3)
    };
    let k2 = if e2 == s2 {
        1 + rng.below(2)
    } else {
        rng.below(3)
    };
    let build = |s: usize, e: usize, k: usize, tag: &str| -> String {
        let mut out: String = (0..s).map(line).collect();
        out.extend((0..k).map(|j| format!("{tag} {j}\n")));
        out.extend((e..n).map(line));
        out
    };
    let (o, t) = (build(s1, e1, k1, "ours"), build(s2, e2, k2, "theirs"));
    if rng.below(2) == 0 {
        (base.into_bytes(), o.into_bytes(), t.into_bytes())
    } else {
        (base.into_bytes(), t.into_bytes(), o.into_bytes())
    }
}

#[test]
fn git_merge_file_differential() {
    if !git_available() {
        eprintln!("SKIP: git not found");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // 1. Contract and hand-written clean examples: must agree.
    let mut tally = Tally::default();
    let mut failures = Vec::new();
    for ex in contract_merge_examples() {
        if let Some(d) = compare(
            dir,
            &mut tally,
            &text(&ex.base),
            &text(&ex.ours),
            &text(&ex.theirs),
        ) {
            failures.push(format!("{}: {d}", ex.name));
        }
    }
    // 2. Curated disjoint edits separated by at least 3 unchanged lines: must agree.
    let mut rng = SplitMix(0x6175);
    for _ in 0..300 {
        let (b, o, t) = curated_case(&mut rng, 3);
        if let Some(d) = compare(dir, &mut tally, &b, &o, &t) {
            failures.push(d);
        }
    }
    eprintln!("curated: {tally:?}");
    assert!(
        failures.is_empty(),
        "unexpected git merge-file disagreements:\n{}",
        failures.join("\n")
    );

    // 3. Random small-alphabet merges: report only (see GIT_MERGE_FILE_DIFFERENCES.md).
    let mut tally = Tally::default();
    let mut samples = Vec::new();
    let alphabet = ["a\n", "b\n", "c\n", "d\n"];
    for _ in 0..300 {
        let b = random_lines(&mut rng, 8, &alphabet);
        let o = random_edit(&mut rng, &b, &alphabet);
        let t = random_edit(&mut rng, &b, &alphabet);
        let (b, o, t) = (b.concat(), o.concat(), t.concat());
        if let Some(d) = compare(dir, &mut tally, b.as_bytes(), o.as_bytes(), t.as_bytes())
            && samples.len() < 5
        {
            samples.push(d);
        }
    }
    eprintln!("random small-alphabet: {tally:?}");
    for s in samples {
        eprintln!("  {s}");
    }
}
