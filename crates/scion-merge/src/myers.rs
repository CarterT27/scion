//! Myers forward greedy O(ND) diff with the normative tie-breaking (line-diff3-v1).
//!
//! This is a direct transcription of `docs/contracts/line-diff3-v1.md` sections 2-4: the forward
//! greedy pass with the fixed k-selection rule, the exact backtracking, then the normalization
//! of every changed run into `Delete` (optional) followed by `Insert` (optional). There are no
//! heuristics, no cost cap and no linear-space variant (SPEC 8.3: the reference tie-breaking is
//! normative for output). To bound memory, the per-round V rows needed by the backtracking are
//! kept as checkpoints and recomputed block by block; this changes no value (the direct
//! transcription is kept as a test oracle).

/// One edit-script operation over line indices of `a` (old) and `b` (new).
///
/// SPEC 8.3 content merge; `docs/contracts/line-diff3-v1.md` section 4.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// `len` equal lines starting at `a` and `b`.
    Equal {
        /// Start in `a`.
        a: usize,
        /// Start in `b`.
        b: usize,
        /// Run length.
        len: usize,
    },
    /// `len` lines of `a` deleted starting at `a`.
    Delete {
        /// Start in `a`.
        a: usize,
        /// Run length.
        len: usize,
    },
    /// `len` lines of `b` inserted starting at `b`.
    Insert {
        /// Start in `b`.
        b: usize,
        /// Run length.
        len: usize,
    },
}

/// A single-line operation produced by backtracking (contract section 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    /// `a[.0] == b[.1]`.
    Equal(usize, usize),
    /// Delete `a[.0]`.
    Delete(usize),
    /// Insert `b[.0]`.
    Insert(usize),
}

/// The edit script from `a` to `b`, EXACTLY as `docs/contracts/line-diff3-v1.md`.
///
/// SPEC 8.3 (Myers with the reference implementation's deterministic tie-breaking). The result
/// is the normalized script of contract section 4: maximal runs, and within every changed run
/// the `Delete` precedes the `Insert`. For two empty inputs the script is empty.
pub fn myers(a: &[&[u8]], b: &[&[u8]]) -> Vec<Edit> {
    normalize(&single_line_script(a, b))
}

/// Rounds per checkpoint block of the production path.
const BLOCK: usize = 64;

/// Contract sections 2 and 3: the forward greedy pass followed by the exact backtracking.
fn single_line_script(a: &[&[u8]], b: &[&[u8]]) -> Vec<Op> {
    blocked_script(a, b, BLOCK)
}

/// Round `d` of the forward pass (contract section 2), computed from the values written in
/// round `d - 1` (`prev[(k + d - 1) / 2] = V[k]` for `k = -(d-1), ..., d-1`; ignored for
/// `d = 0`, where `V_{-1} = {1: 0}`). The selection rule only ever reads round `d - 1` values,
/// so this is exactly the pseudocode's round. Returns the row (`row[(k + d) / 2] = V[k]`) and
/// whether `x >= N and y >= M` was reached (the row is then partial).
fn round(a: &[&[u8]], b: &[&[u8]], prev: &[usize], d: usize) -> (Vec<usize>, bool) {
    let n = a.len();
    let m = b.len();
    let di = d as isize;
    let get = |k: isize| prev.get(((k + di - 1) / 2) as usize).copied().unwrap_or(0);
    let mut row = Vec::with_capacity(d + 1);
    let mut k = -di;
    while k <= di {
        let mut x = if d == 0 {
            0 // V[1] of the initial map
        } else if k == -di || (k != di && get(k - 1) < get(k + 1)) {
            get(k + 1) // DOWN from diagonal k + 1: an insertion
        } else {
            get(k - 1) + 1 // RIGHT from diagonal k - 1: a deletion
        };
        // `y >= 0` always holds on a path that starts at (0, 0).
        let mut y = (x as isize - k) as usize;
        while x < n && y < m && a[x] == b[y] {
            x += 1;
            y += 1;
        }
        row.push(x);
        if x >= n && y >= m {
            return (row, true);
        }
        k += 2;
    }
    (row, false)
}

/// The rows `V_d` needed by the backtracking, kept as one checkpoint row every `block` rounds;
/// the rows of a block are recomputed from its checkpoint when the backtracking reaches it.
/// Memory is O(D^2 / block + block * D) instead of O(D^2); the values are identical.
struct Rows<'s> {
    a: &'s [&'s [u8]],
    b: &'s [&'s [u8]],
    block: usize,
    checkpoints: Vec<Vec<usize>>,
    start: usize,
    rows: Vec<Vec<usize>>,
}

impl Rows<'_> {
    /// `V_r[k]`.
    fn value(&mut self, r: usize, k: isize) -> usize {
        if r < self.start || r >= self.start + self.rows.len() {
            let start = r / self.block * self.block;
            let mut rows: Vec<Vec<usize>> = Vec::with_capacity(self.block);
            if let Some(cp) = self.checkpoints.get(r / self.block) {
                rows.push(cp.clone());
            }
            for d in start + 1..=r.max(start) {
                let next = round(
                    self.a,
                    self.b,
                    rows.last().map_or(&[][..], Vec::as_slice),
                    d,
                )
                .0;
                rows.push(next);
            }
            self.start = start;
            self.rows = rows;
        }
        self.rows
            .get(r - self.start)
            .and_then(|row| row.get(((k + r as isize) / 2) as usize))
            .copied()
            .unwrap_or(0)
    }
}

/// Forward pass with checkpoints every `block` rounds, then the exact backtracking.
fn blocked_script(a: &[&[u8]], b: &[&[u8]], block: usize) -> Vec<Op> {
    let block = block.max(1);
    let max = a.len() + b.len();
    let mut checkpoints: Vec<Vec<usize>> = Vec::new();
    let mut block_rows: Vec<Vec<usize>> = Vec::new();
    let mut block_start = 0usize;
    let mut big_d = max;
    for d in 0..=max {
        let (row, done) = round(a, b, block_rows.last().map_or(&[][..], Vec::as_slice), d);
        if done {
            big_d = d;
            break;
        }
        if d % block == 0 {
            checkpoints.push(row.clone());
            block_rows.clear();
            block_start = d;
        }
        block_rows.push(row);
    }
    let mut rows = Rows {
        a,
        b,
        block,
        checkpoints,
        start: block_start,
        rows: block_rows,
    };
    backtrack(a.len(), b.len(), big_d, |r, k| rows.value(r, k))
}

/// Contract section 3, given `v(r, k) = V_r[k]` for the complete rounds `r < D`.
fn backtrack(
    n: usize,
    m: usize,
    big_d: usize,
    mut v: impl FnMut(usize, isize) -> usize,
) -> Vec<Op> {
    let mut ops = Vec::with_capacity(n + m);
    let mut x = n as isize;
    let mut y = m as isize;
    for d in (0..=big_d).rev() {
        let di = d as isize;
        let k = x - y;
        let (prev_k, prev_x) = if d == 0 {
            // V_{-1} = {1: 0}; at d = 0 we always have k == -d.
            (1isize, 0isize)
        } else {
            let prev_k = if k == -di || (k != di && v(d - 1, k - 1) < v(d - 1, k + 1)) {
                k + 1
            } else {
                k - 1
            };
            (prev_k, v(d - 1, prev_k) as isize)
        };
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Equal((x - 1) as usize, (y - 1) as usize));
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                ops.push(Op::Insert(prev_y as usize));
            } else {
                ops.push(Op::Delete(prev_x as usize));
            }
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    ops
}

/// Direct transcription of contract sections 2 and 3 (full V map, every round kept). Used as
/// the oracle for the memory-bounded production path.
#[cfg(test)]
fn reference_script(a: &[&[u8]], b: &[&[u8]]) -> Vec<Op> {
    let n = a.len();
    let m = b.len();
    let max = n + m;
    // `v[k + off]` holds `V[k]` for `k` in `-(max + 1) ..= max + 1`.
    let off = max + 1;
    let mut v = vec![0usize; 2 * max + 3];
    // `trace[d]` holds `V[k]` after round `d` for `k = -d, -d + 2, ..., d` (only the values
    // written in round `d`, which are the only ones the backtracking reads from that round).
    let mut trace: Vec<Vec<usize>> = Vec::new();
    let mut big_d = 0usize;

    'rounds: for d in 0..=max {
        let di = d as isize;
        let mut row = Vec::with_capacity(d + 1);
        let mut k = -di;
        while k <= di {
            let i = (k + off as isize) as usize;
            let mut x = if k == -di || (k != di && v[i - 1] < v[i + 1]) {
                v[i + 1] // DOWN from diagonal k + 1: an insertion
            } else {
                v[i - 1] + 1 // RIGHT from diagonal k - 1: a deletion
            };
            // `y >= 0` always holds on a path that starts at (0, 0).
            let mut y = (x as isize - k) as usize;
            while x < n && y < m && a[x] == b[y] {
                x += 1;
                y += 1;
            }
            v[i] = x;
            row.push(x);
            if x >= n && y >= m {
                trace.push(row);
                big_d = d;
                break 'rounds;
            }
            k += 2;
        }
        trace.push(row);
    }

    // Backtracking (contract section 3).
    let mut ops = Vec::with_capacity(max);
    let mut x = n as isize;
    let mut y = m as isize;
    for d in (0..=big_d).rev() {
        let di = d as isize;
        let k = x - y;
        let (prev_k, prev_x) = if d == 0 {
            // V_{-1} = {1: 0}; at d = 0 we always have k == -d.
            (1isize, 0isize)
        } else {
            let row = &trace[d - 1];
            let get = |kk: isize| row[((kk + di - 1) / 2) as usize] as isize;
            let prev_k = if k == -di || (k != di && get(k - 1) < get(k + 1)) {
                k + 1
            } else {
                k - 1
            };
            (prev_k, get(prev_k))
        };
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            ops.push(Op::Equal((x - 1) as usize, (y - 1) as usize));
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                ops.push(Op::Insert(prev_y as usize));
            } else {
                ops.push(Op::Delete(prev_x as usize));
            }
        }
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    ops
}

/// Contract section 4: deletions before insertions within each changed run, then coalesce.
fn normalize(ops: &[Op]) -> Vec<Edit> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < ops.len() {
        if let Op::Equal(a0, b0) = ops[i] {
            let mut len = 0;
            while let Some(Op::Equal(..)) = ops.get(i) {
                len += 1;
                i += 1;
            }
            out.push(Edit::Equal { a: a0, b: b0, len });
            continue;
        }
        let mut del: Option<(usize, usize)> = None;
        let mut ins: Option<(usize, usize)> = None;
        while let Some(op) = ops.get(i) {
            match *op {
                Op::Equal(..) => break,
                Op::Delete(a0) => match del.as_mut() {
                    Some((_, len)) => *len += 1,
                    None => del = Some((a0, 1)),
                },
                Op::Insert(b0) => match ins.as_mut() {
                    Some((_, len)) => *len += 1,
                    None => ins = Some((b0, 1)),
                },
            }
            i += 1;
        }
        if let Some((a, len)) = del {
            out.push(Edit::Delete { a, len });
        }
        if let Some((b, len)) = ins {
            out.push(Edit::Insert { b, len });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &[&'static str]) -> Vec<&'static [u8]> {
        s.iter().map(|l| l.as_bytes()).collect()
    }

    #[test]
    fn d4_single_line_script_matches_trace() {
        let a = lines(&["A\n", "B\n"]);
        let b = lines(&["B\n", "A\n"]);
        assert_eq!(
            single_line_script(&a, &b),
            vec![Op::Delete(0), Op::Equal(1, 0), Op::Insert(1)]
        );
    }

    #[test]
    fn empty() {
        assert!(myers(&[], &[]).is_empty());
        assert!(reference_script(&[], &[]).is_empty());
    }

    /// The checkpointed production path equals the direct transcription for every block size.
    #[test]
    fn blocked_equals_reference() {
        let alphabet: [&[u8]; 3] = [b"a\n", b"b\n", b"c\n"];
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = move |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as usize
        };
        for _ in 0..3000 {
            let na = next(40);
            let nb = next(40);
            let a: Vec<&[u8]> = (0..na).map(|_| alphabet[next(3)]).collect();
            let b: Vec<&[u8]> = (0..nb).map(|_| alphabet[next(3)]).collect();
            let want = reference_script(&a, &b);
            for block in [1, 2, 3, 5, 8, 64] {
                assert_eq!(blocked_script(&a, &b, block), want, "block {block}");
            }
        }
        // Large D with the default block: many checkpoints and recomputed blocks.
        let a: Vec<String> = (0..700).map(|i| format!("a{i}\n")).collect();
        let b: Vec<String> = (0..650).map(|i| format!("b{i}\n")).collect();
        let a: Vec<&[u8]> = a.iter().map(|s| s.as_bytes()).collect();
        let b: Vec<&[u8]> = b.iter().map(|s| s.as_bytes()).collect();
        assert_eq!(single_line_script(&a, &b), reference_script(&a, &b));
    }

    #[test]
    fn interleaved_run_is_reordered() {
        // A changed run whose raw script interleaves inserts and deletes.
        let a = lines(&["a\n", "b\n", "c\n"]);
        let b = lines(&["x\n", "y\n", "c\n"]);
        assert_eq!(
            myers(&a, &b),
            vec![
                Edit::Delete { a: 0, len: 2 },
                Edit::Insert { b: 0, len: 2 },
                Edit::Equal { a: 2, b: 2, len: 1 }
            ]
        );
    }
}
