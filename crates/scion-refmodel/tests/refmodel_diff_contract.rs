//! Every worked example of `docs/contracts/line-diff3-v1.md` section 10, reproduced exactly by
//! the model's own Myers and diff3 (SPEC 8.3, 19.4).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use scion_refmodel::diff::{diff_hunks, edit_runs, merge3, merge3_full, myers, split_lines};

fn lines<'a>(ls: &[&'a str]) -> Vec<&'a [u8]> {
    ls.iter().map(|l| l.as_bytes()).collect()
}

/// Render the edit runs in the contract's notation.
fn edits(a: &[&str], b: &[&str]) -> String {
    let runs = edit_runs(&lines(a), &lines(b));
    let parts: Vec<String> = runs
        .iter()
        .map(|&(op, x, y, len)| match op {
            '=' => format!("Equal{{a:{x}, b:{y}, len:{len}}}"),
            '-' => format!("Delete{{a:{x}, len:{len}}}"),
            _ => format!("Insert{{b:{y}, len:{len}}}"),
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

fn hunks(a: &[&str], b: &[&str]) -> String {
    let parts: Vec<String> = diff_hunks(&lines(a), &lines(b))
        .iter()
        .map(|h| format!("{{base: {}..{}, new: {}..{}}}", h.bs, h.be, h.ns, h.ne))
        .collect();
    format!("[{}]", parts.join(", "))
}

fn check_diff(a: &[&str], b: &[&str], want_edits: &str, want_hunks: &str) {
    assert_eq!(edits(a, b), want_edits, "edits of {a:?} -> {b:?}");
    assert_eq!(hunks(a, b), want_hunks, "hunks of {a:?} -> {b:?}");
}

#[test]
fn d4_trace_script() {
    let a = lines(&["A\n", "B\n"]);
    let b = lines(&["B\n", "A\n"]);
    assert_eq!(myers(&a, &b), vec![('-', 0, 0), ('=', 1, 0), ('+', 2, 1)]);
}

#[test]
fn d1_to_d12() {
    check_diff(&[], &[], "[]", "[]");
    check_diff(
        &[],
        &["x\n"],
        "[Insert{b:0, len:1}]",
        "[{base: 0..0, new: 0..1}]",
    );
    check_diff(
        &["x\n"],
        &[],
        "[Delete{a:0, len:1}]",
        "[{base: 0..1, new: 0..0}]",
    );
    check_diff(
        &["A\n", "B\n"],
        &["B\n", "A\n"],
        "[Delete{a:0, len:1}, Equal{a:1, b:0, len:1}, Insert{b:1, len:1}]",
        "[{base: 0..1, new: 0..0}, {base: 2..2, new: 1..2}]",
    );
    check_diff(
        &["A\n", "B\n", "C\n"],
        &["A\n", "C\n"],
        "[Equal{a:0, b:0, len:1}, Delete{a:1, len:1}, Equal{a:2, b:1, len:1}]",
        "[{base: 1..2, new: 1..1}]",
    );
    check_diff(
        &["a\n", "a\n", "a\n"],
        &["a\n", "a\n"],
        "[Equal{a:0, b:0, len:2}, Delete{a:2, len:1}]",
        "[{base: 2..3, new: 2..2}]",
    );
    check_diff(
        &["x\n"],
        &["x\n", "x\n"],
        "[Equal{a:0, b:0, len:1}, Insert{b:1, len:1}]",
        "[{base: 1..1, new: 1..2}]",
    );
    check_diff(
        &["a\n", "b\n", "a\n", "b\n"],
        &["b\n", "a\n", "b\n", "a\n"],
        "[Delete{a:0, len:1}, Equal{a:1, b:0, len:3}, Insert{b:3, len:1}]",
        "[{base: 0..1, new: 0..0}, {base: 4..4, new: 3..4}]",
    );
    check_diff(
        &["x\n", "y"],
        &["x\n", "y\n"],
        "[Equal{a:0, b:0, len:1}, Delete{a:1, len:1}, Insert{b:1, len:1}]",
        "[{base: 1..2, new: 1..2}]",
    );
    check_diff(
        &["l1\r\n", "l2\r\n"],
        &["l1\n", "l2\r\n"],
        "[Delete{a:0, len:1}, Insert{b:0, len:1}, Equal{a:1, b:1, len:1}]",
        "[{base: 0..1, new: 0..1}]",
    );
    check_diff(
        &["a\n", "b\n"],
        &["c\n", "d\n"],
        "[Delete{a:0, len:2}, Insert{b:0, len:2}]",
        "[{base: 0..2, new: 0..2}]",
    );
    check_diff(
        &["A\n", "B\n", "C\n", "A\n", "B\n", "B\n", "A\n"],
        &["C\n", "B\n", "A\n", "B\n", "A\n", "C\n"],
        "[Delete{a:0, len:2}, Equal{a:2, b:0, len:1}, Insert{b:1, len:1}, Equal{a:3, b:2, len:2}, Delete{a:5, len:1}, Equal{a:6, b:4, len:1}, Insert{b:5, len:1}]",
        "[{base: 0..2, new: 0..0}, {base: 3..3, new: 1..2}, {base: 5..6, new: 4..4}, {base: 7..7, new: 5..6}]",
    );
}

fn join(ls: &[&str]) -> Vec<u8> {
    ls.concat().into_bytes()
}

fn clean(base: &[&str], ours: &[&str], theirs: &[&str], want: &str) {
    let r = merge3(&join(base), &join(ours), &join(theirs));
    assert_eq!(
        r,
        Ok(want.as_bytes().to_vec()),
        "{base:?} {ours:?} {theirs:?}"
    );
}

type Ranges = ((usize, usize), (usize, usize), (usize, usize));

fn conflict(base: &[&str], ours: &[&str], theirs: &[&str], want: &[Ranges]) {
    let r = merge3_full(&join(base), &join(ours), &join(theirs));
    assert_eq!(r, Err(want.to_vec()), "{base:?} {ours:?} {theirs:?}");
    let short = merge3(&join(base), &join(ours), &join(theirs));
    assert_eq!(short, Err(want.iter().map(|w| w.0).collect()));
}

#[test]
fn m1_to_m12() {
    clean(
        &["a\n", "b\n", "c\n"],
        &["A\n", "b\n", "c\n"],
        &["a\n", "b\n", "C\n"],
        "A\nb\nC\n",
    );
    conflict(
        &["a\n", "b\n", "c\n"],
        &["a\n", "X\n", "c\n"],
        &["a\n", "Y\n", "c\n"],
        &[((1, 2), (1, 2), (1, 2))],
    );
    clean(
        &["a\n", "b\n", "c\n"],
        &["a\n", "X\n", "c\n"],
        &["a\n", "X\n", "c\n"],
        "a\nX\nc\n",
    );
    conflict(
        &["a\n", "b\n", "c\n", "d\n"],
        &["a\n", "B\n", "c\n", "d\n"],
        &["a\n", "b\n", "C\n", "d\n"],
        &[((1, 3), (1, 3), (1, 3))],
    );
    clean(
        &["1\n", "2\n", "3\n", "4\n", "5\n"],
        &["1\n", "X\n", "3\n", "4\n", "5\n"],
        &["1\n", "2\n", "3\n", "Y\n", "5\n"],
        "1\nX\n3\nY\n5\n",
    );
    clean(
        &["a\n", "b\n"],
        &["a\n", "X\n", "b\n"],
        &["a\n", "b\n", "Y\n"],
        "a\nX\nb\nY\n",
    );
    clean(
        &["a\n", "b\n"],
        &["X\n", "a\n", "b\n"],
        &["a\n", "b\n"],
        "X\na\nb\n",
    );
    conflict(&[], &["x\n"], &["y\n"], &[((0, 0), (0, 1), (0, 1))]);
    clean(&[], &["x\n"], &["x\n"], "x\n");
    conflict(
        &["a\n", "b\n", "c\n"],
        &["a\n", "c\n"],
        &["a\n", "b\n", "B\n", "c\n"],
        &[((1, 2), (1, 1), (1, 3))],
    );
    clean(&["a\n", "b"], &["a\n", "b\n"], &["a\n", "b"], "a\nb\n");
    clean(
        &["a\r\n", "b\r\n"],
        &["a\n", "b\r\n"],
        &["a\r\n", "b\r\n", "c\r\n"],
        "a\nb\r\nc\r\n",
    );
}

#[test]
fn merge_consequences() {
    // ours == base gives theirs exactly, and vice versa; ours == theirs gives ours.
    let b = b"one\ntwo\nthree";
    let x = b"zero\none\n2\nthree\n";
    assert_eq!(merge3(b, b, x), Ok(x.to_vec()));
    assert_eq!(merge3(b, x, b), Ok(x.to_vec()));
    assert_eq!(merge3(b, x, x), Ok(x.to_vec()));
    assert_eq!(split_lines(b).len(), 3);
}
