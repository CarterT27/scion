//! The model's own Myers and diff3, written from `docs/contracts/line-diff3-v1.md`
//! (SPEC 8.3 content merge, SPEC 9.2 blame line mapping).
//!
//! This is a direct, unoptimized transcription of the contract: the forward greedy O(ND) search
//! keeps a full snapshot of `V` per round, backtracking replays the same selection rule, and the
//! three-way merge sweeps hunks into regions exactly as contract section 6 describes.

use std::collections::BTreeMap;

/// Split on `\n` keeping terminators (line-diff3-v1 section 1; SPEC 8.3).
///
/// A final unterminated line is a line; the empty input has zero lines.
pub fn split_lines(b: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &c) in b.iter().enumerate() {
        if c == b'\n' {
            out.push(&b[start..=i]);
            start = i + 1;
        }
    }
    if start < b.len() {
        out.push(&b[start..]);
    }
    out
}

/// Read `V[k]`; the selection rule only reads diagonals written in the previous round, so the
/// default is never observed for a valid read.
fn vget(v: &BTreeMap<i64, i64>, k: i64) -> i64 {
    v.get(&k).copied().unwrap_or(0)
}

/// One single-line operation of the raw (unnormalized) Myers script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RawOp {
    Equal(usize, usize),
    Delete(usize),
    Insert(usize),
}

/// Contract sections 2 and 3: the raw single-line edit script, in forward order.
fn myers_raw(a: &[&[u8]], b: &[&[u8]]) -> Vec<RawOp> {
    let n = a.len() as i64;
    let m = b.len() as i64;
    let max = n + m;
    let mut v: BTreeMap<i64, i64> = BTreeMap::new();
    v.insert(1, 0);
    let mut trace: Vec<BTreeMap<i64, i64>> = Vec::new();
    let mut big_d: i64 = 0;
    'outer: for d in 0..=max {
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && vget(&v, k - 1) < vget(&v, k + 1)) {
                vget(&v, k + 1)
            } else {
                vget(&v, k - 1) + 1
            };
            let mut y = x - k;
            while x < n && y < m && x >= 0 && y >= 0 && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v.insert(k, x);
            if x >= n && y >= m {
                trace.push(v.clone());
                big_d = d;
                break 'outer;
            }
            k += 2;
        }
        trace.push(v.clone());
    }

    // Backtracking (section 3).
    let mut initial: BTreeMap<i64, i64> = BTreeMap::new();
    initial.insert(1, 0);
    let mut ops = Vec::new();
    let (mut x, mut y) = (n, m);
    let mut d = big_d;
    while d >= 0 {
        let vprev = if d == 0 {
            &initial
        } else {
            match trace.get((d - 1) as usize) {
                Some(t) => t,
                None => &initial,
            }
        };
        let k = x - y;
        let prev_k = if k == -d || (k != d && vget(vprev, k - 1) < vget(vprev, k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let prev_x = vget(vprev, prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(RawOp::Equal((x - 1) as usize, (y - 1) as usize));
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                ops.push(RawOp::Insert(prev_y as usize));
            } else {
                ops.push(RawOp::Delete(prev_x as usize));
            }
        }
        x = prev_x;
        y = prev_y;
        d -= 1;
    }
    ops.reverse();
    ops
}

/// Edit script as `(op, a_index, b_index)` with op `'='`, `'-'` or `'+'`, one entry per line
/// (line-diff3-v1 sections 2-4; SPEC 8.3).
///
/// The script is normalized: within every changed run all deletions precede all insertions.
/// Indices: `'='` carries the equal lines' indices in `a` and `b`; `'-'` carries the deleted
/// line's index in `a` and the position in `b` before which the run sits; `'+'` carries the
/// position in `a` after the run's deletions and the inserted line's index in `b`.
pub fn myers(a: &[&[u8]], b: &[&[u8]]) -> Vec<(char, usize, usize)> {
    let raw = myers_raw(a, b);
    let mut out = Vec::new();
    let (mut x, mut y) = (0usize, 0usize);
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            RawOp::Equal(ai, bi) => {
                out.push(('=', ai, bi));
                x = ai + 1;
                y = bi + 1;
                i += 1;
            }
            _ => {
                // A changed run: collect deletions and insertions in their original order.
                let mut dels = Vec::new();
                let mut inss = Vec::new();
                while i < raw.len() {
                    match raw[i] {
                        RawOp::Equal(..) => break,
                        RawOp::Delete(ai) => dels.push(ai),
                        RawOp::Insert(bi) => inss.push(bi),
                    }
                    i += 1;
                }
                for &ai in &dels {
                    out.push(('-', ai, y));
                }
                let xa = x + dels.len();
                for &bi in &inss {
                    out.push(('+', xa, bi));
                }
                x = xa;
                y += inss.len();
            }
        }
    }
    out
}

/// A hunk of a two-way diff: base range `bs..be`, new range `ns..ne` (contract section 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MHunk {
    /// Base range start.
    pub bs: usize,
    /// Base range end (exclusive).
    pub be: usize,
    /// New range start.
    pub ns: usize,
    /// New range end (exclusive).
    pub ne: usize,
}

/// `diff_lines(a, b)`: one hunk per changed run of the normalized script (contract section 5).
pub fn diff_hunks(a: &[&[u8]], b: &[&[u8]]) -> Vec<MHunk> {
    let script = myers(a, b);
    let mut hunks = Vec::new();
    let (mut x, mut y) = (0usize, 0usize);
    let mut i = 0;
    while i < script.len() {
        let (op, ai, bi) = script[i];
        if op == '=' {
            x = ai + 1;
            y = bi + 1;
            i += 1;
            continue;
        }
        let (x0, y0) = (x, y);
        let (mut nd, mut ni) = (0usize, 0usize);
        while i < script.len() && script[i].0 != '=' {
            if script[i].0 == '-' {
                nd += 1;
            } else {
                ni += 1;
            }
            i += 1;
        }
        hunks.push(MHunk {
            bs: x0,
            be: x0 + nd,
            ns: y0,
            ne: y0 + ni,
        });
        x = x0 + nd;
        y = y0 + ni;
    }
    hunks
}

/// The edit runs of the normalized script as `(op, a_start, b_start, len)` (contract section 4,
/// the `Edit` runs): `'='` is `Equal{a, b, len}`, `'-'` is `Delete{a, len}` (with `b` the
/// current `b` position), `'+'` is `Insert{b, len}` (with `a` the current `a` position).
pub fn edit_runs(a: &[&[u8]], b: &[&[u8]]) -> Vec<(char, usize, usize, usize)> {
    let mut runs: Vec<(char, usize, usize, usize)> = Vec::new();
    for (op, ai, bi) in myers(a, b) {
        if let Some(last) = runs.last_mut() {
            let contiguous = match op {
                '=' => last.0 == '=' && last.1 + last.3 == ai && last.2 + last.3 == bi,
                '-' => last.0 == '-' && last.1 + last.3 == ai,
                _ => last.0 == '+' && last.2 + last.3 == bi,
            };
            if contiguous {
                last.3 += 1;
                continue;
            }
        }
        runs.push((op, ai, bi, 1));
    }
    runs
}

/// For each line of `child`, the `parent` line it maps to through an equal run, if any
/// (contract section 9 `line_origins`; SPEC 9.2 blame).
pub fn line_origins(parent: &[u8], child: &[u8]) -> Vec<Option<usize>> {
    let p = split_lines(parent);
    let c = split_lines(child);
    let mut out = vec![None; c.len()];
    for (op, ai, bi) in myers(&p, &c) {
        if op == '='
            && let Some(slot) = out.get_mut(bi)
        {
            *slot = Some(ai);
        }
    }
    out
}

/// A region of the three-way merge: base range and the hunks of each side in it.
struct Region {
    rs: usize,
    re: usize,
    ours: Vec<MHunk>,
    theirs: Vec<MHunk>,
}

/// `text_S` of a region for one side (contract section 6 step 4).
fn side_text<'a>(
    base: &[&'a [u8]],
    side: &[&'a [u8]],
    hs: &[MHunk],
    rs: usize,
    re: usize,
) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    let mut pos = rs;
    for h in hs {
        out.extend_from_slice(&base[pos..h.bs]);
        out.extend_from_slice(&side[h.ns..h.ne]);
        pos = h.be;
    }
    out.extend_from_slice(&base[pos..re]);
    out
}

/// `range_S` of a side within a conflicting region (contract section 6 step 4). Conflicting
/// regions always carry hunks from both sides.
fn side_range(hs: &[MHunk], rs: usize, re: usize) -> (usize, usize) {
    match (hs.first(), hs.last()) {
        (Some(f), Some(l)) => (f.ns - (f.bs - rs), l.ne + (re - l.be)),
        _ => (rs, re),
    }
}

/// Full three-way merge result: merged bytes, or conflict hunks
/// `((base_start, base_end), (ours_start, ours_end), (theirs_start, theirs_end))`
/// (contract section 6; SPEC 8.3).
#[allow(clippy::type_complexity)]
pub fn merge3_full(
    b: &[u8],
    o: &[u8],
    t: &[u8],
) -> Result<Vec<u8>, Vec<((usize, usize), (usize, usize), (usize, usize))>> {
    let bl = split_lines(b);
    let ol = split_lines(o);
    let tl = split_lines(t);
    let ho = diff_hunks(&bl, &ol);
    let ht = diff_hunks(&bl, &tl);
    // (hunk, side) with side 0 = ours, 1 = theirs; sorted by (start, end).
    let mut all: Vec<(MHunk, u8)> = ho
        .iter()
        .map(|h| (*h, 0u8))
        .chain(ht.iter().map(|h| (*h, 1u8)))
        .collect();
    all.sort_by_key(|(h, s)| (h.bs, h.be, *s));
    let mut regions: Vec<Region> = Vec::new();
    for (h, s) in all {
        let join = match regions.last() {
            Some(r) => h.bs <= r.re,
            None => false,
        };
        if !join {
            regions.push(Region {
                rs: h.bs,
                re: h.be,
                ours: Vec::new(),
                theirs: Vec::new(),
            });
        }
        if let Some(r) = regions.last_mut() {
            r.re = r.re.max(h.be);
            if s == 0 {
                r.ours.push(h);
            } else {
                r.theirs.push(h);
            }
        }
    }
    let mut out: Vec<u8> = Vec::new();
    let mut conflicts = Vec::new();
    let mut pos = 0usize;
    for r in &regions {
        for line in &bl[pos..r.rs] {
            out.extend_from_slice(line);
        }
        let text_o = side_text(&bl, &ol, &r.ours, r.rs, r.re);
        let text_t = side_text(&bl, &tl, &r.theirs, r.rs, r.re);
        if r.theirs.is_empty() {
            for line in &text_o {
                out.extend_from_slice(line);
            }
        } else if r.ours.is_empty() || text_o == text_t {
            for line in &text_t {
                out.extend_from_slice(line);
            }
        } else {
            conflicts.push((
                (r.rs, r.re),
                side_range(&r.ours, r.rs, r.re),
                side_range(&r.theirs, r.rs, r.re),
            ));
        }
        pos = r.re;
    }
    for line in &bl[pos..] {
        out.extend_from_slice(line);
    }
    if conflicts.is_empty() {
        Ok(out)
    } else {
        Err(conflicts)
    }
}

/// line-diff3-v1 merge: merged bytes, or conflicting base line ranges `(start, end)`
/// (contract section 6; SPEC 8.3 content merge).
pub fn merge3(b: &[u8], o: &[u8], t: &[u8]) -> Result<Vec<u8>, Vec<(usize, usize)>> {
    merge3_full(b, o, t).map_err(|cs| cs.into_iter().map(|(base, _, _)| base).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_lines_rules() {
        assert!(split_lines(b"").is_empty());
        assert_eq!(split_lines(b"\n"), vec![&b"\n"[..]]);
        assert_eq!(split_lines(b"a"), vec![&b"a"[..]]);
        assert_eq!(split_lines(b"a\n"), vec![&b"a\n"[..]]);
        assert_eq!(split_lines(b"a\nb"), vec![&b"a\n"[..], &b"b"[..]]);
        assert_eq!(split_lines(b"x\r\ny"), vec![&b"x\r\n"[..], &b"y"[..]]);
    }

    #[test]
    fn line_origins_basic() {
        assert_eq!(
            line_origins(b"a\nb\nc\n", b"a\nX\nc\n"),
            vec![Some(0), None, Some(2)]
        );
        assert_eq!(line_origins(b"", b"a\n"), vec![None]);
    }
}
