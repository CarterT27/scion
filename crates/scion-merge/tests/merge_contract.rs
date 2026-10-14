//! Worked examples of `docs/contracts/line-diff3-v1.md` section 10 (normative), plus the
//! section 6 consequences, SPEC 8.3 / 19.4.

mod merge_support;

use merge_support::*;
use scion_merge::{Merge3, diff_lines, line_origins, merge3, myers, render_markers, split_lines};

#[test]
fn contract_diff_examples_10_2() {
    for ex in contract_diff_examples() {
        let a = lines(&ex.a);
        let b = lines(&ex.b);
        assert_eq!(myers(&a, &b), ex.edits, "{} edits", ex.name);
        assert_eq!(diff_lines(&a, &b), ex.hunks, "{} hunks", ex.name);
        check_script(&a, &b, &ex.edits);
        // Splitting the concatenation gives back the example's lines.
        assert_eq!(split_lines(&text(&ex.a)), a, "{} split", ex.name);
    }
}

#[test]
fn contract_merge_examples_10_3() {
    for ex in contract_merge_examples() {
        let got = merge3(&text(&ex.base), &text(&ex.ours), &text(&ex.theirs));
        assert_eq!(got, ex.result, "{}", ex.name);
    }
}

#[test]
fn contract_d4_trace_10_1_keeps_b() {
    // The tie rule of section 2 is observable: B is kept and A moves.
    let a = lines(&["A\n", "B\n"]);
    let b = lines(&["B\n", "A\n"]);
    assert_eq!(myers(&a, &b), vec![del(0, 1), eq(1, 0, 1), ins(1, 1)]);
    assert_eq!(line_origins(b"A\nB\n", b"B\nA\n"), vec![Some(1), None]);
}

#[test]
fn merge_is_symmetric_on_examples() {
    for ex in contract_merge_examples() {
        let fwd = merge3(&text(&ex.base), &text(&ex.ours), &text(&ex.theirs));
        let rev = merge3(&text(&ex.base), &text(&ex.theirs), &text(&ex.ours));
        match (fwd, rev) {
            (Merge3::Clean(x), Merge3::Clean(y)) => assert_eq!(x, y, "{}", ex.name),
            (Merge3::Conflict(x), Merge3::Conflict(y)) => {
                let swapped: Vec<_> = y
                    .into_iter()
                    .map(|h| ch(h.base, h.theirs, h.ours))
                    .collect();
                assert_eq!(x, swapped, "{}", ex.name);
            }
            (x, y) => panic!("{}: asymmetric outcome {x:?} vs {y:?}", ex.name),
        }
    }
}

#[test]
fn markers_for_contract_conflicts() {
    // M2
    assert_eq!(
        render_markers(b"a\nb\nc\n", b"a\nX\nc\n", b"a\nY\nc\n"),
        b"a\n<<<<<<< ours\nX\n||||||| base\nb\n=======\nY\n>>>>>>> theirs\nc\n".to_vec()
    );
    // M8: empty base region.
    assert_eq!(
        render_markers(b"", b"x\n", b"y\n"),
        b"<<<<<<< ours\nx\n||||||| base\n=======\ny\n>>>>>>> theirs\n".to_vec()
    );
    // M10: deletion vs insertion, the ours text is empty.
    assert_eq!(
        render_markers(b"a\nb\nc\n", b"a\nc\n", b"a\nb\nB\nc\n"),
        b"a\n<<<<<<< ours\n||||||| base\nb\n=======\nb\nB\n>>>>>>> theirs\nc\n".to_vec()
    );
    // Clean examples render exactly like merge3.
    for ex in contract_merge_examples() {
        if let Merge3::Clean(bytes) = &ex.result {
            assert_eq!(
                &render_markers(&text(&ex.base), &text(&ex.ours), &text(&ex.theirs)),
                bytes,
                "{}",
                ex.name
            );
        }
    }
}

#[test]
fn markers_mixed_clean_and_conflict_regions() {
    let base = b"1\n2\n3\n4\n5\n";
    let ours = b"one\n2\n3\nX\n5\n";
    let theirs = b"1\n2\n3\nY\n5";
    assert_eq!(
        merge3(base, ours, theirs),
        Merge3::Conflict(vec![ch(3..5, 3..5, 3..5)])
    );
    assert_eq!(
        render_markers(base, ours, theirs),
        b"one\n2\n3\n<<<<<<< ours\nX\n5\n||||||| base\n4\n5\n=======\nY\n5\n>>>>>>> theirs\n"
            .to_vec()
    );
}
