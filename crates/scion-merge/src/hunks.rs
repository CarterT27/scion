//! Hunk extraction from the Myers edit script (line-diff3-v1).

use crate::myers::{Edit, myers};
use std::ops::Range;

/// A changed region: `base` lines replaced by `new` lines.
///
/// SPEC 8.3; `docs/contracts/line-diff3-v1.md` section 5.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// Replaced range in the old text.
    pub base: Range<usize>,
    /// Replacement range in the new text.
    pub new: Range<usize>,
}

/// Group the non-equal runs of `myers(a, b)` into hunks.
///
/// SPEC 8.3; `docs/contracts/line-diff3-v1.md` section 5: one hunk per changed run, in ascending
/// order. A pure insertion has an empty `base` range, a pure deletion an empty `new` range, and
/// two hunks are always separated by at least one equal line.
pub fn diff_lines(a: &[&[u8]], b: &[&[u8]]) -> Vec<Hunk> {
    hunks_of(&myers(a, b))
}

/// Hunks of an already normalized edit script.
pub(crate) fn hunks_of(edits: &[Edit]) -> Vec<Hunk> {
    let mut out = Vec::new();
    let mut x = 0usize;
    let mut y = 0usize;
    let mut cur: Option<Hunk> = None;
    for e in edits {
        match *e {
            Edit::Equal { a, b, len } => {
                if let Some(h) = cur.take() {
                    out.push(h);
                }
                x = a + len;
                y = b + len;
            }
            Edit::Delete { a, len } => {
                cur = Some(Hunk {
                    base: a..a + len,
                    new: y..y,
                });
                x = a + len;
            }
            Edit::Insert { b, len } => {
                match cur.as_mut() {
                    Some(h) => h.new = b..b + len,
                    None => {
                        cur = Some(Hunk {
                            base: x..x,
                            new: b..b + len,
                        })
                    }
                }
                y = b + len;
            }
        }
    }
    if let Some(h) = cur {
        out.push(h);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d12_hunks() {
        let a: Vec<&[u8]> = ["A\n", "B\n", "C\n", "A\n", "B\n", "B\n", "A\n"]
            .iter()
            .map(|s| s.as_bytes())
            .collect();
        let b: Vec<&[u8]> = ["C\n", "B\n", "A\n", "B\n", "A\n", "C\n"]
            .iter()
            .map(|s| s.as_bytes())
            .collect();
        assert_eq!(
            diff_lines(&a, &b),
            vec![
                Hunk {
                    base: 0..2,
                    new: 0..0
                },
                Hunk {
                    base: 3..3,
                    new: 1..2
                },
                Hunk {
                    base: 5..6,
                    new: 4..4
                },
                Hunk {
                    base: 7..7,
                    new: 5..6
                },
            ]
        );
    }
}
