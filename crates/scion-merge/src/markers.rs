//! Conflict marker rendering for working copies (SPEC 11.7). Never used for server objects.

use crate::merge3::{Piece, analyze};

// The marker opening the `ours` side of a conflict (SPEC 11.7).
const MARKER_OURS: &[u8] = b"<<<<<<< ours\n";
// The marker opening the `base` side of a conflict (SPEC 11.7).
const MARKER_BASE: &[u8] = b"||||||| base\n";
// The marker opening the `theirs` side of a conflict (SPEC 11.7).
const MARKER_SEP: &[u8] = b"=======\n";
// The marker closing a conflict (SPEC 11.7).
const MARKER_THEIRS: &[u8] = b">>>>>>> theirs\n";

/// Render markers `<<<<<<< ours`, `||||||| base`, `=======`, `>>>>>>> theirs` around the
/// conflicting regions of a content merge.
///
/// SPEC 11.7; `docs/contracts/line-diff3-v1.md` section 8. Resolved regions are emitted exactly
/// as `merge3` would; a conflicting region becomes `<<<<<<< ours`, its ours text,
/// `||||||| base`, the base lines of the region, `=======`, its theirs text and
/// `>>>>>>> theirs`. A `\n` is inserted before a marker line only when the output so far is
/// non-empty and does not end in `\n`. Without conflicts the result equals the clean merge.
pub fn render_markers(base: &[u8], ours: &[u8], theirs: &[u8]) -> Vec<u8> {
    let analysis = analyze(base, ours, theirs);
    let mut out = Vec::with_capacity(base.len().max(ours.len()).max(theirs.len()));
    let push_lines = |out: &mut Vec<u8>, lines: &[&[u8]]| {
        for l in lines {
            out.extend_from_slice(l);
        }
    };
    let marker = |out: &mut Vec<u8>, m: &[u8]| {
        if out.last().is_some_and(|&c| c != b'\n') {
            out.push(b'\n');
        }
        out.extend_from_slice(m);
    };
    analysis.assemble(|piece| match piece {
        Piece::Lines(lines) => push_lines(&mut out, lines),
        Piece::Conflict { base, ours, theirs } => {
            marker(&mut out, MARKER_OURS);
            push_lines(&mut out, ours);
            marker(&mut out, MARKER_BASE);
            push_lines(&mut out, base);
            marker(&mut out, MARKER_SEP);
            push_lines(&mut out, theirs);
            marker(&mut out, MARKER_THEIRS);
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m2_markers() {
        let out = render_markers(b"a\nb\nc\n", b"a\nX\nc\n", b"a\nY\nc\n");
        assert_eq!(
            out,
            b"a\n<<<<<<< ours\nX\n||||||| base\nb\n=======\nY\n>>>>>>> theirs\nc\n".to_vec()
        );
    }

    #[test]
    fn unterminated_sides_get_a_newline_before_markers() {
        let out = render_markers(b"a\nb", b"a\nX", b"a\nY");
        assert_eq!(
            out,
            b"a\n<<<<<<< ours\nX\n||||||| base\nb\n=======\nY\n>>>>>>> theirs\n".to_vec()
        );
    }

    #[test]
    fn clean_equals_merge() {
        assert_eq!(
            render_markers(b"a\nb\nc\n", b"A\nb\nc\n", b"a\nb\nC\n"),
            b"A\nb\nC\n".to_vec()
        );
    }
}
