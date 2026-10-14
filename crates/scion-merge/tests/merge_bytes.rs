//! Byte exactness (CRLF, unterminated final lines), determinism, blame line origins and unified
//! hunks (SPEC 8.3, 9.2, 7.4, 19.4).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod merge_support;

use merge_support::*;
use scion_merge::{
    ENGINE, Merge3, TextHunk, line_origins, merge3, myers, render_markers, split_lines,
    unified_hunks,
};

#[test]
fn engine_name() {
    assert_eq!(ENGINE, "line-diff3-v1");
}

#[test]
fn crlf_merge_is_byte_exact() {
    let base = b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n";
    let ours = b"ONE\r\ntwo\r\nthree\r\nfour\r\nfive\r\n";
    let theirs = b"one\r\ntwo\r\nthree\r\nfour\r\nFIVE\r\n";
    assert_eq!(
        merge3(base, ours, theirs),
        Merge3::Clean(b"ONE\r\ntwo\r\nthree\r\nfour\r\nFIVE\r\n".to_vec())
    );
    // Mixed endings are kept exactly as each side supplied them.
    let ours = b"one\ntwo\r\nthree\r\nfour\r\nfive\r\n";
    let theirs = b"one\r\ntwo\r\nthree\r\nfour\nfive\r\n";
    assert_eq!(
        merge3(base, ours, theirs),
        Merge3::Clean(b"one\ntwo\r\nthree\r\nfour\nfive\r\n".to_vec())
    );
    // A lone CR is an ordinary byte.
    assert_eq!(
        merge3(b"a\rb\nc\n", b"a\rb\nC\n", b"A\rb\nc\n"),
        Merge3::Conflict(vec![ch(0..2, 0..2, 0..2)])
    );
    assert_eq!(split_lines(b"a\rb\n"), vec![&b"a\rb\n"[..]]);
}

#[test]
fn unterminated_final_line_is_byte_exact() {
    // The unterminated final line survives a change elsewhere.
    assert_eq!(
        merge3(b"a\nb\nc\nd", b"A\nb\nc\nd", b"a\nb\nc\nd"),
        Merge3::Clean(b"A\nb\nc\nd".to_vec())
    );
    assert_eq!(
        merge3(b"a\nb\nc\nd", b"A\nb\nc\nd", b"a\nb\nC\nd"),
        Merge3::Clean(b"A\nb\nC\nd".to_vec())
    );
    // One side drops the terminator of the last line, the other edits line 0.
    assert_eq!(
        merge3(b"a\nb\nc\nd\n", b"A\nb\nc\nd\n", b"a\nb\nc\nd"),
        Merge3::Clean(b"A\nb\nc\nd".to_vec())
    );
    // Both drop it: applied once.
    assert_eq!(
        merge3(b"a\nb\n", b"a\nb", b"a\nb"),
        Merge3::Clean(b"a\nb".to_vec())
    );
    // Empty results are exact too.
    assert_eq!(merge3(b"x", b"", b"x"), Merge3::Clean(Vec::new()));
    // Markers start on their own line after an unterminated side text.
    assert_eq!(
        render_markers(b"a\nb", b"a\nB", b"a\nC"),
        b"a\n<<<<<<< ours\nB\n||||||| base\nb\n=======\nC\n>>>>>>> theirs\n".to_vec()
    );
}

#[test]
fn determinism_same_input_same_output() {
    let mut rng = SplitMix(42);
    let alphabet = ["a\n", "b\n", "c\n", "a\r\n", "z"];
    for _ in 0..500 {
        let b = random_lines(&mut rng, 10, &alphabet).concat().into_bytes();
        let o = random_lines(&mut rng, 10, &alphabet).concat().into_bytes();
        let t = random_lines(&mut rng, 10, &alphabet).concat().into_bytes();
        let first = (merge3(&b, &o, &t), render_markers(&b, &o, &t));
        for _ in 0..3 {
            assert_eq!(first, (merge3(&b, &o, &t), render_markers(&b, &o, &t)));
        }
        let (bl, ol) = (split_lines(&b), split_lines(&o));
        assert_eq!(myers(&bl, &ol), myers(&bl, &ol));
        assert_eq!(line_origins(&b, &o), line_origins(&b, &o));
        assert_eq!(unified_hunks(&b, &o, 3), unified_hunks(&b, &o, 3));
    }
}

#[test]
fn large_inputs_are_handled() {
    // 2000 lines with scattered edits; checks the O(ND) path on a non-trivial size.
    let base: String = (0..2000).map(|i| format!("line {i}\n")).collect();
    let ours: String = (0..2000)
        .map(|i| {
            if i % 100 == 0 {
                format!("ours {i}\n")
            } else {
                format!("line {i}\n")
            }
        })
        .collect();
    let theirs: String = (0..2000)
        .map(|i| {
            if i % 100 == 50 {
                format!("theirs {i}\n")
            } else {
                format!("line {i}\n")
            }
        })
        .collect();
    let want: String = (0..2000)
        .map(|i| match i % 100 {
            0 => format!("ours {i}\n"),
            50 => format!("theirs {i}\n"),
            _ => format!("line {i}\n"),
        })
        .collect();
    assert_eq!(
        merge3(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()),
        Merge3::Clean(want.into_bytes())
    );
}

#[test]
fn line_origins_for_blame() {
    // Every child line is either mapped through an Equal run or new.
    let parent = b"a\nb\nc\nd\n";
    let child = b"a\nX\nc\nd\nY\n";
    assert_eq!(
        line_origins(parent, child),
        vec![Some(0), None, Some(2), Some(3), None]
    );
    // Repeated lines follow the tie-breaking (D6, D7).
    assert_eq!(
        line_origins(b"a\na\na\n", b"a\na\n"),
        vec![Some(0), Some(1)]
    );
    assert_eq!(line_origins(b"x\n", b"x\nx\n"), vec![Some(0), None]);
    // CRLF vs LF lines never match.
    assert_eq!(line_origins(b"l1\r\n", b"l1\n"), vec![None]);
    // Mapped parent indices are strictly increasing.
    let mut rng = SplitMix(7);
    for _ in 0..200 {
        let p = random_lines(&mut rng, 12, &["a\n", "b\n", "c\n"]).concat();
        let c = random_lines(&mut rng, 12, &["a\n", "b\n", "c\n"]).concat();
        let origins = line_origins(p.as_bytes(), c.as_bytes());
        assert_eq!(origins.len(), split_lines(c.as_bytes()).len());
        let mapped: Vec<usize> = origins.iter().flatten().copied().collect();
        assert!(mapped.windows(2).all(|w| w[0] < w[1]));
        let (pl, cl) = (split_lines(p.as_bytes()), split_lines(c.as_bytes()));
        for (j, o) in origins.iter().enumerate() {
            if let Some(i) = o {
                assert_eq!(pl[*i], cl[j]);
            }
        }
        assert_eq!(mapped.len(), lcs_len(&pl, &cl));
    }
}

fn tl(tag: u8, s: &str) -> (u8, Vec<u8>) {
    (tag, s.as_bytes().to_vec())
}

#[test]
fn unified_hunks_shapes() {
    // Replacement listed as deletions then insertions, with 3 lines of context.
    let a = b"1\n2\n3\n4\n5\n6\n7\n8\n";
    let b = b"1\n2\n3\nx\ny\n6\n7\n8\n";
    assert_eq!(
        unified_hunks(a, b, 3),
        vec![TextHunk {
            old_start: 1,
            old_lines: 8,
            new_start: 1,
            new_lines: 8,
            lines: vec![
                tl(b' ', "1\n"),
                tl(b' ', "2\n"),
                tl(b' ', "3\n"),
                tl(b'-', "4\n"),
                tl(b'-', "5\n"),
                tl(b'+', "x\n"),
                tl(b'+', "y\n"),
                tl(b' ', "6\n"),
                tl(b' ', "7\n"),
                tl(b' ', "8\n"),
            ],
        }]
    );
    // Delete everything.
    assert_eq!(
        unified_hunks(b"a\nb\n", b"", 3),
        vec![TextHunk {
            old_start: 1,
            old_lines: 2,
            new_start: 0,
            new_lines: 0,
            lines: vec![tl(b'-', "a\n"), tl(b'-', "b\n")],
        }]
    );
    // Unterminated lines are carried verbatim.
    assert_eq!(
        unified_hunks(b"x\ny", b"x\ny\n", 1),
        vec![TextHunk {
            old_start: 1,
            old_lines: 2,
            new_start: 1,
            new_lines: 2,
            lines: vec![tl(b' ', "x\n"), tl(b'-', "y"), tl(b'+', "y\n")],
        }]
    );
    assert!(unified_hunks(b"", b"", 3).is_empty());
}

#[test]
fn unified_hunks_reconstruct_both_sides() {
    let mut rng = SplitMix(99);
    let alphabet = ["a\n", "b\n", "c\n", "d\n"];
    for _ in 0..300 {
        let a = random_lines(&mut rng, 20, &alphabet);
        let b = random_edit(&mut rng, &a, &alphabet);
        let (ab, bb) = (a.concat().into_bytes(), b.concat().into_bytes());
        for context in [0usize, 1, 3] {
            let hunks = unified_hunks(&ab, &bb, context);
            let al = split_lines(&ab);
            let bl = split_lines(&bb);
            let mut old_pos = 0usize; // lines of `a` consumed
            let mut rebuilt: Vec<Vec<u8>> = Vec::new();
            for h in &hunks {
                let old_first = if h.old_lines == 0 {
                    h.old_start as usize
                } else {
                    h.old_start as usize - 1
                };
                assert!(old_first >= old_pos, "hunks overlap or are out of order");
                rebuilt.extend(al[old_pos..old_first].iter().map(|l| l.to_vec()));
                let (mut n_old, mut n_new) = (0u32, 0u32);
                for (tag, line) in &h.lines {
                    match tag {
                        b' ' => {
                            n_old += 1;
                            n_new += 1;
                            rebuilt.push(line.clone());
                        }
                        b'-' => n_old += 1,
                        b'+' => {
                            n_new += 1;
                            rebuilt.push(line.clone());
                        }
                        _ => panic!("bad tag"),
                    }
                }
                assert_eq!((n_old, n_new), (h.old_lines, h.new_lines));
                old_pos = old_first + h.old_lines as usize;
            }
            rebuilt.extend(al[old_pos..].iter().map(|l| l.to_vec()));
            assert_eq!(rebuilt, bl.iter().map(|l| l.to_vec()).collect::<Vec<_>>());
        }
    }
}
