//! Shared helpers for the scion-merge integration tests (P02).
#![allow(dead_code)]

use scion_core::ConflictHunk;
use scion_merge::{Edit, Hunk, Merge3};

/// Lines from string literals.
pub fn lines<'a>(s: &[&'a str]) -> Vec<&'a [u8]> {
    s.iter().map(|l| l.as_bytes()).collect()
}

/// Concatenate string lines into bytes.
pub fn text(s: &[&str]) -> Vec<u8> {
    s.concat().into_bytes()
}

/// Edit distance (insertions plus deletions) of a script.
pub fn script_cost(edits: &[Edit]) -> usize {
    edits
        .iter()
        .map(|e| match e {
            Edit::Equal { .. } => 0,
            Edit::Delete { len, .. } | Edit::Insert { len, .. } => *len,
        })
        .sum()
}

/// O(N*M) longest common subsequence length.
pub fn lcs_len(a: &[&[u8]], b: &[&[u8]]) -> usize {
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

/// Checks that `edits` is a well-formed normalized script transforming `a` into `b`
/// (contract section 4) and returns the rebuilt `b`.
pub fn check_script(a: &[&[u8]], b: &[&[u8]], edits: &[Edit]) {
    let (mut x, mut y) = (0usize, 0usize);
    let mut out: Vec<&[u8]> = Vec::new();
    let mut prev: Option<&Edit> = None;
    for e in edits {
        match *e {
            Edit::Equal { a: ea, b: eb, len } => {
                assert!(len >= 1);
                assert_eq!((ea, eb), (x, y), "equal run out of place");
                assert!(
                    !matches!(prev, Some(Edit::Equal { .. })),
                    "uncoalesced equal"
                );
                for i in 0..len {
                    assert_eq!(a[ea + i], b[eb + i], "equal run pairs different lines");
                    out.push(a[ea + i]);
                }
                x += len;
                y += len;
            }
            Edit::Delete { a: ea, len } => {
                assert!(len >= 1);
                assert_eq!(ea, x);
                assert!(
                    matches!(prev, None | Some(Edit::Equal { .. })),
                    "delete must start a changed run"
                );
                x += len;
            }
            Edit::Insert { b: eb, len } => {
                assert!(len >= 1);
                assert_eq!(eb, y);
                assert!(
                    !matches!(prev, Some(Edit::Insert { .. })),
                    "uncoalesced insert"
                );
                out.extend_from_slice(&b[eb..eb + len]);
                y += len;
            }
        }
        prev = Some(e);
    }
    assert_eq!(
        (x, y),
        (a.len(), b.len()),
        "script does not consume both inputs"
    );
    assert_eq!(out, b);
}

/// Shorthand constructors.
pub fn eq(a: usize, b: usize, len: usize) -> Edit {
    Edit::Equal { a, b, len }
}
pub fn del(a: usize, len: usize) -> Edit {
    Edit::Delete { a, len }
}
pub fn ins(b: usize, len: usize) -> Edit {
    Edit::Insert { b, len }
}
pub fn hunk(base: std::ops::Range<usize>, new: std::ops::Range<usize>) -> Hunk {
    Hunk { base, new }
}
pub fn ch(
    base: std::ops::Range<u32>,
    ours: std::ops::Range<u32>,
    theirs: std::ops::Range<u32>,
) -> ConflictHunk {
    ConflictHunk { base, ours, theirs }
}

/// A diff worked example from `docs/contracts/line-diff3-v1.md` section 10.2.
pub struct DiffExample {
    pub name: &'static str,
    pub description: &'static str,
    pub a: Vec<&'static str>,
    pub b: Vec<&'static str>,
    pub edits: Vec<Edit>,
    pub hunks: Vec<Hunk>,
}

/// A merge worked example from `docs/contracts/line-diff3-v1.md` section 10.3.
pub struct MergeExample {
    pub name: &'static str,
    pub description: &'static str,
    pub base: Vec<&'static str>,
    pub ours: Vec<&'static str>,
    pub theirs: Vec<&'static str>,
    pub result: Merge3,
}

/// Contract section 10.2, transcribed literally.
pub fn contract_diff_examples() -> Vec<DiffExample> {
    vec![
        DiffExample {
            name: "D1",
            description: "empty inputs",
            a: vec![],
            b: vec![],
            edits: vec![],
            hunks: vec![],
        },
        DiffExample {
            name: "D2",
            description: "insert into empty",
            a: vec![],
            b: vec!["x\n"],
            edits: vec![ins(0, 1)],
            hunks: vec![hunk(0..0, 0..1)],
        },
        DiffExample {
            name: "D3",
            description: "delete everything",
            a: vec!["x\n"],
            b: vec![],
            edits: vec![del(0, 1)],
            hunks: vec![hunk(0..1, 0..0)],
        },
        DiffExample {
            name: "D4",
            description: "swap AB/BA (tie: the deletion branch wins, so B is kept)",
            a: vec!["A\n", "B\n"],
            b: vec!["B\n", "A\n"],
            edits: vec![del(0, 1), eq(1, 0, 1), ins(1, 1)],
            hunks: vec![hunk(0..1, 0..0), hunk(2..2, 1..2)],
        },
        DiffExample {
            name: "D5",
            description: "delete a middle line",
            a: vec!["A\n", "B\n", "C\n"],
            b: vec!["A\n", "C\n"],
            edits: vec![eq(0, 0, 1), del(1, 1), eq(2, 1, 1)],
            hunks: vec![hunk(1..2, 1..1)],
        },
        DiffExample {
            name: "D6",
            description: "repeated lines: the LAST copy is deleted",
            a: vec!["a\n", "a\n", "a\n"],
            b: vec!["a\n", "a\n"],
            edits: vec![eq(0, 0, 2), del(2, 1)],
            hunks: vec![hunk(2..3, 2..2)],
        },
        DiffExample {
            name: "D7",
            description: "repeated lines: an inserted copy goes at the END",
            a: vec!["x\n"],
            b: vec!["x\n", "x\n"],
            edits: vec![eq(0, 0, 1), ins(1, 1)],
            hunks: vec![hunk(1..1, 1..2)],
        },
        DiffExample {
            name: "D8",
            description: "rotation of a repeated pair",
            a: vec!["a\n", "b\n", "a\n", "b\n"],
            b: vec!["b\n", "a\n", "b\n", "a\n"],
            edits: vec![del(0, 1), eq(1, 0, 3), ins(3, 1)],
            hunks: vec![hunk(0..1, 0..0), hunk(4..4, 3..4)],
        },
        DiffExample {
            name: "D9",
            description: "unterminated last line differs from the terminated one",
            a: vec!["x\n", "y"],
            b: vec!["x\n", "y\n"],
            edits: vec![eq(0, 0, 1), del(1, 1), ins(1, 1)],
            hunks: vec![hunk(1..2, 1..2)],
        },
        DiffExample {
            name: "D10",
            description: "CRLF lines differ from LF lines",
            a: vec!["l1\r\n", "l2\r\n"],
            b: vec!["l1\n", "l2\r\n"],
            edits: vec![del(0, 1), ins(0, 1), eq(1, 1, 1)],
            hunks: vec![hunk(0..1, 0..1)],
        },
        DiffExample {
            name: "D11",
            description: "replacement: deletions listed before insertions",
            a: vec!["a\n", "b\n"],
            b: vec!["c\n", "d\n"],
            edits: vec![del(0, 2), ins(0, 2)],
            hunks: vec![hunk(0..2, 0..2)],
        },
        DiffExample {
            name: "D12",
            description: "Myers paper example ABCABBA -> CBABAC",
            a: vec!["A\n", "B\n", "C\n", "A\n", "B\n", "B\n", "A\n"],
            b: vec!["C\n", "B\n", "A\n", "B\n", "A\n", "C\n"],
            edits: vec![
                del(0, 2),
                eq(2, 0, 1),
                ins(1, 1),
                eq(3, 2, 2),
                del(5, 1),
                eq(6, 4, 1),
                ins(5, 1),
            ],
            hunks: vec![
                hunk(0..2, 0..0),
                hunk(3..3, 1..2),
                hunk(5..6, 4..4),
                hunk(7..7, 5..6),
            ],
        },
    ]
}

/// Contract section 10.3, transcribed literally.
pub fn contract_merge_examples() -> Vec<MergeExample> {
    let clean = |s: &str| Merge3::Clean(s.as_bytes().to_vec());
    vec![
        MergeExample {
            name: "M1",
            description: "non-overlapping edits on both sides",
            base: vec!["a\n", "b\n", "c\n"],
            ours: vec!["A\n", "b\n", "c\n"],
            theirs: vec!["a\n", "b\n", "C\n"],
            result: clean("A\nb\nC\n"),
        },
        MergeExample {
            name: "M2",
            description: "same line changed differently: conflict",
            base: vec!["a\n", "b\n", "c\n"],
            ours: vec!["a\n", "X\n", "c\n"],
            theirs: vec!["a\n", "Y\n", "c\n"],
            result: Merge3::Conflict(vec![ch(1..2, 1..2, 1..2)]),
        },
        MergeExample {
            name: "M3",
            description: "identical change on both sides: applied once",
            base: vec!["a\n", "b\n", "c\n"],
            ours: vec!["a\n", "X\n", "c\n"],
            theirs: vec!["a\n", "X\n", "c\n"],
            result: clean("a\nX\nc\n"),
        },
        MergeExample {
            name: "M4",
            description: "adjacent changed lines (zero unchanged lines between): conflict",
            base: vec!["a\n", "b\n", "c\n", "d\n"],
            ours: vec!["a\n", "B\n", "c\n", "d\n"],
            theirs: vec!["a\n", "b\n", "C\n", "d\n"],
            result: Merge3::Conflict(vec![ch(1..3, 1..3, 1..3)]),
        },
        MergeExample {
            name: "M5",
            description: "separated by one unchanged line: clean",
            base: vec!["1\n", "2\n", "3\n", "4\n", "5\n"],
            ours: vec!["1\n", "X\n", "3\n", "4\n", "5\n"],
            theirs: vec!["1\n", "2\n", "3\n", "Y\n", "5\n"],
            result: clean("1\nX\n3\nY\n5\n"),
        },
        MergeExample {
            name: "M6",
            description: "insertions at different points",
            base: vec!["a\n", "b\n"],
            ours: vec!["a\n", "X\n", "b\n"],
            theirs: vec!["a\n", "b\n", "Y\n"],
            result: clean("a\nX\nb\nY\n"),
        },
        MergeExample {
            name: "M7",
            description: "one-sided change only",
            base: vec!["a\n", "b\n"],
            ours: vec!["X\n", "a\n", "b\n"],
            theirs: vec!["a\n", "b\n"],
            result: clean("X\na\nb\n"),
        },
        MergeExample {
            name: "M8",
            description: "both sides add different content to an empty base: conflict",
            base: vec![],
            ours: vec!["x\n"],
            theirs: vec!["y\n"],
            result: Merge3::Conflict(vec![ch(0..0, 0..1, 0..1)]),
        },
        MergeExample {
            name: "M9",
            description: "both sides add identical content to an empty base",
            base: vec![],
            ours: vec!["x\n"],
            theirs: vec!["x\n"],
            result: clean("x\n"),
        },
        MergeExample {
            name: "M10",
            description: "an insertion touching the end of a deletion: conflict",
            base: vec!["a\n", "b\n", "c\n"],
            ours: vec!["a\n", "c\n"],
            theirs: vec!["a\n", "b\n", "B\n", "c\n"],
            result: Merge3::Conflict(vec![ch(1..2, 1..1, 1..3)]),
        },
        MergeExample {
            name: "M11",
            description: "unterminated last line gains a terminator on one side",
            base: vec!["a\n", "b"],
            ours: vec!["a\n", "b\n"],
            theirs: vec!["a\n", "b"],
            result: clean("a\nb\n"),
        },
        MergeExample {
            name: "M12",
            description: "CRLF preserved byte for byte",
            base: vec!["a\r\n", "b\r\n"],
            ours: vec!["a\n", "b\r\n"],
            theirs: vec!["a\r\n", "b\r\n", "c\r\n"],
            result: clean("a\nb\r\nc\r\n"),
        },
    ]
}

/// Deterministic SplitMix64 generator for seeded fixtures (no OS randomness).
pub struct SplitMix(pub u64);

impl SplitMix {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (n > 0).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// A random line sequence over a small alphabet (many repeats, so tie-breaks matter).
pub fn random_lines(rng: &mut SplitMix, max_len: usize, alphabet: &[&'static str]) -> Vec<String> {
    let n = rng.below(max_len + 1);
    (0..n)
        .map(|_| alphabet[rng.below(alphabet.len())].to_string())
        .collect()
}

/// Random line-level edits of `base`: deletions, insertions and replacements.
pub fn random_edit(rng: &mut SplitMix, base: &[String], alphabet: &[&'static str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        match rng.below(6) {
            0 if i < base.len() => i += 1, // delete
            1 => out.push(alphabet[rng.below(alphabet.len())].to_string()), // insert
            2 if i < base.len() => {
                out.push(alphabet[rng.below(alphabet.len())].to_string()); // replace
                i += 1;
            }
            _ => {
                if i < base.len() {
                    out.push(base[i].clone());
                    i += 1;
                } else {
                    break;
                }
            }
        }
        if i >= base.len() && rng.below(3) == 0 {
            break;
        }
    }
    out
}
