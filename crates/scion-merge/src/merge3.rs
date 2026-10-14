//! line-diff3-v1 three-way content merge (SPEC 8.3).
//!
//! Transcription of `docs/contracts/line-diff3-v1.md` section 6: hunks of both sides, regions
//! as the transitive closure of overlapping or touching base ranges, one-sided regions applied,
//! identical two-sided regions applied once, anything else a conflict.

use crate::hunks::{Hunk, diff_lines};
use crate::split::split_lines;
use scion_core::ConflictHunk;
use std::ops::Range;

/// Result of a content merge.
///
/// SPEC 8.3: conflicts are returned as data (line ranges), never as markers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Merge3 {
    /// Byte-exact merged content.
    Clean(Vec<u8>),
    /// Conflicting regions as base/ours/theirs line ranges.
    Conflict(Vec<ConflictHunk>),
}

/// Merge `ours` and `theirs` against `base` (no CRLF normalization; unterminated final lines
/// preserved).
///
/// SPEC 8.3 content merge (`merge_engine = "line-diff3-v1"`), exactly as
/// `docs/contracts/line-diff3-v1.md` section 6. Conflict hunks carry 0-based half-open line
/// ranges into `split_lines(base)`, `split_lines(ours)` and `split_lines(theirs)`.
pub fn merge3(base: &[u8], ours: &[u8], theirs: &[u8]) -> Merge3 {
    let analysis = analyze(base, ours, theirs);
    let conflicts: Vec<ConflictHunk> = analysis
        .regions
        .iter()
        .filter_map(|r| match &r.outcome {
            Outcome::Conflict { hunk, .. } => Some(hunk.clone()),
            Outcome::Resolved(_) => None,
        })
        .collect();
    if !conflicts.is_empty() {
        return Merge3::Conflict(conflicts);
    }
    let mut out = Vec::with_capacity(base.len().max(ours.len()).max(theirs.len()));
    analysis.assemble(|piece| match piece {
        Piece::Lines(lines) => {
            for l in lines {
                out.extend_from_slice(l);
            }
        }
        Piece::Conflict { .. } => {}
    });
    Merge3::Clean(out)
}

/// Which side a hunk came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Ours,
    Theirs,
}

/// The decision for one region (contract section 6.5).
#[derive(Debug)]
pub(crate) enum Outcome<'a> {
    /// The region's replacement lines.
    Resolved(Vec<&'a [u8]>),
    /// A conflict with both side texts.
    Conflict {
        hunk: ConflictHunk,
        ours: Vec<&'a [u8]>,
        theirs: Vec<&'a [u8]>,
    },
}

/// One region: base range `[rs, re)` and its outcome.
#[derive(Debug)]
pub(crate) struct Region<'a> {
    pub(crate) base: Range<usize>,
    pub(crate) outcome: Outcome<'a>,
}

/// The base lines plus the regions of a merge, in base order.
#[derive(Debug)]
pub(crate) struct Analysis<'a> {
    pub(crate) base: Vec<&'a [u8]>,
    pub(crate) regions: Vec<Region<'a>>,
}

/// A piece of the assembled output (contract section 6.6).
pub(crate) enum Piece<'r, 'a> {
    /// Lines copied verbatim (unchanged base lines or a resolved region).
    Lines(&'r [&'a [u8]]),
    /// A conflicting region.
    Conflict {
        base: &'r [&'a [u8]],
        ours: &'r [&'a [u8]],
        theirs: &'r [&'a [u8]],
    },
}

impl<'a> Analysis<'a> {
    /// Visit the output pieces in order: base lines before, between and after regions, and each
    /// region's replacement (contract section 6.6).
    pub(crate) fn assemble<'r>(&'r self, mut f: impl FnMut(Piece<'r, 'a>)) {
        let mut pos = 0usize;
        for r in &self.regions {
            if let Some(pre) = self.base.get(pos..r.base.start)
                && !pre.is_empty()
            {
                f(Piece::Lines(pre));
            }
            match &r.outcome {
                Outcome::Resolved(lines) => f(Piece::Lines(lines)),
                Outcome::Conflict { ours, theirs, .. } => f(Piece::Conflict {
                    base: self.base.get(r.base.clone()).unwrap_or(&[]),
                    ours,
                    theirs,
                }),
            }
            pos = r.base.end;
        }
        if let Some(rest) = self.base.get(pos..)
            && !rest.is_empty()
        {
            f(Piece::Lines(rest));
        }
    }
}

/// Saturating `usize -> u32` for conflict line ranges (inputs are bounded by
/// `limits.merge_max_text_bytes`, far below `u32::MAX` lines).
fn r32(r: Range<usize>) -> Range<u32> {
    let c = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
    c(r.start)..c(r.end)
}

/// Contract section 6.4: the text of side `S` over region `[rs, re)` and its line range in `S`.
fn side_text<'a>(
    base: &[&'a [u8]],
    side: &[&'a [u8]],
    hunks: &[&Hunk],
    rs: usize,
    re: usize,
) -> (Vec<&'a [u8]>, Range<usize>) {
    let mut text = Vec::new();
    let mut pos = rs;
    for h in hunks {
        text.extend_from_slice(base.get(pos..h.base.start).unwrap_or(&[]));
        text.extend_from_slice(side.get(h.new.clone()).unwrap_or(&[]));
        pos = h.base.end;
    }
    text.extend_from_slice(base.get(pos..re).unwrap_or(&[]));
    let range = match (hunks.first(), hunks.last()) {
        (Some(first), Some(last)) => {
            first.new.start.saturating_sub(first.base.start - rs)
                ..last.new.end + (re - last.base.end)
        }
        _ => rs..re,
    };
    (text, range)
}

/// Contract section 6 steps 1-5: hunks, ordering, regions and per-region decisions.
pub(crate) fn analyze<'a>(base: &'a [u8], ours: &'a [u8], theirs: &'a [u8]) -> Analysis<'a> {
    let b = split_lines(base);
    let o = split_lines(ours);
    let t = split_lines(theirs);
    let ho = diff_lines(&b, &o);
    let ht = diff_lines(&b, &t);

    let mut all: Vec<(Side, &Hunk)> = ho
        .iter()
        .map(|h| (Side::Ours, h))
        .chain(ht.iter().map(|h| (Side::Theirs, h)))
        .collect();
    all.sort_by_key(|(_, h)| (h.base.start, h.base.end));

    // Sweep into regions: a hunk joins the current region iff its start <= the region's end.
    let mut groups: Vec<(usize, usize, Vec<(Side, &Hunk)>)> = Vec::new();
    for (side, h) in all {
        match groups.last_mut() {
            Some((_, re, members)) if h.base.start <= *re => {
                *re = (*re).max(h.base.end);
                members.push((side, h));
            }
            _ => groups.push((h.base.start, h.base.end, vec![(side, h)])),
        }
    }

    let mut regions = Vec::with_capacity(groups.len());
    for (rs, re, members) in groups {
        let oh: Vec<&Hunk> = members
            .iter()
            .filter(|(s, _)| *s == Side::Ours)
            .map(|(_, h)| *h)
            .collect();
        let th: Vec<&Hunk> = members
            .iter()
            .filter(|(s, _)| *s == Side::Theirs)
            .map(|(_, h)| *h)
            .collect();
        let outcome = if th.is_empty() {
            Outcome::Resolved(side_text(&b, &o, &oh, rs, re).0)
        } else if oh.is_empty() {
            Outcome::Resolved(side_text(&b, &t, &th, rs, re).0)
        } else {
            let (ot, orange) = side_text(&b, &o, &oh, rs, re);
            let (tt, trange) = side_text(&b, &t, &th, rs, re);
            if ot == tt {
                Outcome::Resolved(ot)
            } else {
                Outcome::Conflict {
                    hunk: ConflictHunk {
                        base: r32(rs..re),
                        ours: r32(orange),
                        theirs: r32(trange),
                    },
                    ours: ot,
                    theirs: tt,
                }
            }
        };
        regions.push(Region {
            base: rs..re,
            outcome,
        });
    }
    Analysis { base: b, regions }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m2_conflict() {
        assert_eq!(
            merge3(b"a\nb\nc\n", b"a\nX\nc\n", b"a\nY\nc\n"),
            Merge3::Conflict(vec![ConflictHunk {
                base: 1..2,
                ours: 1..2,
                theirs: 1..2
            }])
        );
    }

    #[test]
    fn m1_clean() {
        assert_eq!(
            merge3(b"a\nb\nc\n", b"A\nb\nc\n", b"a\nb\nC\n"),
            Merge3::Clean(b"A\nb\nC\n".to_vec())
        );
    }

    #[test]
    fn all_empty() {
        assert_eq!(merge3(b"", b"", b""), Merge3::Clean(Vec::new()));
    }
}
