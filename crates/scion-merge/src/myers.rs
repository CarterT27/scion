//! Myers forward greedy O(ND) diff with the normative tie-breaking (line-diff3-v1).
//!
//! This is a direct transcription of `docs/contracts/line-diff3-v1.md` sections 2-4: the forward
//! greedy pass with the fixed k-selection rule, the exact backtracking, then the normalization
//! of every changed run into `Delete` (optional) followed by `Insert` (optional). There are no
//! heuristics, no cost cap and no linear-space variant (SPEC 8.3: the reference tie-breaking is
//! normative for output).

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

/// Contract sections 2 and 3: the forward greedy pass followed by the exact backtracking.
fn single_line_script(a: &[&[u8]], b: &[&[u8]]) -> Vec<Op> {
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
