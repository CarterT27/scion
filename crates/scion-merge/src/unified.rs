//! Unified diff hunks for the Diff RPC (SPEC 7.4).

use crate::hunks::diff_lines;
use crate::split::split_lines;

/// One unified-diff hunk (1-based starts as in `@@ -a,b +c,d @@`).
///
/// SPEC 7.4 `Diff(..., include_content_hunks)`; `docs/contracts/line-diff3-v1.md` section 9.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextHunk {
    /// Start line in the old text.
    pub old_start: u32,
    /// Line count in the old text.
    pub old_lines: u32,
    /// Start line in the new text.
    pub new_start: u32,
    /// Line count in the new text.
    pub new_lines: u32,
    /// Lines prefixed by `b' '`, `b'-'` or `b'+'`.
    pub lines: Vec<(u8, Vec<u8>)>,
}

fn u32_sat(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// GNU-style start: the 1-based number of the first covered line, or, when the side covers no
/// line, the number of the line after which the change occurs.
fn start_of(lo: usize, count: usize) -> u32 {
    if count == 0 {
        u32_sat(lo)
    } else {
        u32_sat(lo + 1)
    }
}

/// Unified hunks between `a` and `b` with `context` lines of context.
///
/// SPEC 7.4; `docs/contracts/line-diff3-v1.md` section 9. Changed runs of the normalized Myers
/// script that are separated by at most `2 * context` equal lines share a hunk; each hunk adds up
/// to `context` equal lines before and after. Lines keep their terminators; within a changed run
/// deletions precede insertions.
pub fn unified_hunks(a: &[u8], b: &[u8], context: usize) -> Vec<TextHunk> {
    let al = split_lines(a);
    let bl = split_lines(b);
    let hunks = diff_lines(&al, &bl);
    // `context` may be arbitrarily large (e.g. "whole file"); never overflow.
    let join_gap = context.saturating_mul(2);

    // Group consecutive changed runs.
    let mut groups: Vec<std::ops::Range<usize>> = Vec::new();
    for (i, h) in hunks.iter().enumerate() {
        match groups.last_mut() {
            Some(g)
                if hunks
                    .get(g.end - 1)
                    .is_some_and(|prev| h.base.start - prev.base.end <= join_gap) =>
            {
                g.end = i + 1;
            }
            _ => groups.push(i..i + 1),
        }
    }

    let mut out = Vec::with_capacity(groups.len());
    for g in groups {
        let (Some(first), Some(last)) = (hunks.get(g.start), hunks.get(g.end - 1)) else {
            continue;
        };
        let pre = context.min(first.base.start);
        let post = context.min(al.len() - last.base.end);
        let old_lo = first.base.start - pre;
        let old_hi = last.base.end + post;
        let new_lo = first.new.start - pre;
        let new_hi = last.new.end + post;

        let mut lines: Vec<(u8, Vec<u8>)> = Vec::new();
        let mut push = |tag: u8, src: &[&[u8]]| {
            for l in src {
                lines.push((tag, l.to_vec()));
            }
        };
        let mut pos = old_lo;
        for h in hunks.get(g.clone()).unwrap_or(&[]) {
            push(b' ', al.get(pos..h.base.start).unwrap_or(&[]));
            push(b'-', al.get(h.base.clone()).unwrap_or(&[]));
            push(b'+', bl.get(h.new.clone()).unwrap_or(&[]));
            pos = h.base.end;
        }
        push(b' ', al.get(pos..old_hi).unwrap_or(&[]));

        out.push(TextHunk {
            old_start: start_of(old_lo, old_hi - old_lo),
            old_lines: u32_sat(old_hi - old_lo),
            new_start: start_of(new_lo, new_hi - new_lo),
            new_lines: u32_sat(new_hi - new_lo),
            lines,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(tag: u8, s: &str) -> (u8, Vec<u8>) {
        (tag, s.as_bytes().to_vec())
    }

    #[test]
    fn single_change_with_context() {
        let a = b"1\n2\n3\n4\n5\n6\n7\n";
        let b = b"1\n2\n3\nX\n5\n6\n7\n";
        let h = unified_hunks(a, b, 1);
        assert_eq!(
            h,
            vec![TextHunk {
                old_start: 3,
                old_lines: 3,
                new_start: 3,
                new_lines: 3,
                lines: vec![
                    l(b' ', "3\n"),
                    l(b'-', "4\n"),
                    l(b'+', "X\n"),
                    l(b' ', "5\n")
                ],
            }]
        );
    }

    #[test]
    fn gnu_starts_for_empty_sides() {
        assert_eq!(
            unified_hunks(b"", b"x\n", 3),
            vec![TextHunk {
                old_start: 0,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
                lines: vec![l(b'+', "x\n")],
            }]
        );
        // Pure insertion after line 2 with no context: "@@ -2,0 +3 @@".
        let h = unified_hunks(b"a\nb\nc\n", b"a\nb\nX\nc\n", 0);
        assert_eq!((h[0].old_start, h[0].old_lines), (2, 0));
        assert_eq!((h[0].new_start, h[0].new_lines), (3, 1));
        // Pure deletion of line 2 with no context: "@@ -2 +1,0 @@".
        let h = unified_hunks(b"a\nb\nc\n", b"a\nc\n", 0);
        assert_eq!((h[0].old_start, h[0].old_lines), (2, 1));
        assert_eq!((h[0].new_start, h[0].new_lines), (1, 0));
    }

    #[test]
    fn grouping_by_context() {
        let a = b"1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        // Changes at lines 2 and 6: 3 equal lines between.
        let b = b"1\nX\n3\n4\n5\nY\n7\n8\n9\n";
        assert_eq!(unified_hunks(a, b, 1).len(), 2);
        let merged = unified_hunks(a, b, 2);
        assert_eq!(merged.len(), 1);
        assert_eq!((merged[0].old_start, merged[0].old_lines), (1, 8));
        assert!(unified_hunks(a, a, 3).is_empty());
    }

    /// A "whole file" context (`usize::MAX`) must not overflow `2 * context`: everything is one
    /// hunk covering both texts entirely.
    #[test]
    fn huge_context_does_not_overflow() {
        let a = b"1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let b = b"1\nX\n3\n4\n5\n6\n7\nY\n9\n";
        let h = unified_hunks(a, b, usize::MAX);
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].old_start, h[0].old_lines), (1, 9));
        assert_eq!((h[0].new_start, h[0].new_lines), (1, 9));
        assert_eq!(h[0].lines.len(), 11);
    }
}
