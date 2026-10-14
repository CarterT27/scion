//! Line origin mapping for blame (SPEC 9.2).

use crate::myers::{Edit, myers};
use crate::split::split_lines;

/// For each line of `child`, the matching line index in `parent` (via `diff_lines`), or `None`
/// for lines introduced by the child.
///
/// SPEC 9.2 `blame`; `docs/contracts/line-diff3-v1.md` section 9: a child line maps to a parent
/// line exactly when the normalized Myers script pairs them in an `Equal` run.
pub fn line_origins(parent: &[u8], child: &[u8]) -> Vec<Option<usize>> {
    let p = split_lines(parent);
    let c = split_lines(child);
    let mut out = vec![None; c.len()];
    for e in myers(&p, &c) {
        if let Edit::Equal { a, b, len } = e {
            for i in 0..len {
                if let Some(slot) = out.get_mut(b + i) {
                    *slot = Some(a + i);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        assert_eq!(
            line_origins(b"a\nb\nc\n", b"a\nX\nc\nd\n"),
            vec![Some(0), None, Some(2), None]
        );
        assert!(line_origins(b"a\n", b"").is_empty());
        assert_eq!(line_origins(b"", b"x"), vec![None]);
        // D4: B is kept, A is new.
        assert_eq!(line_origins(b"A\nB\n", b"B\nA\n"), vec![Some(1), None]);
    }
}
