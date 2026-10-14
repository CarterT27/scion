//! Line origin mapping for blame (SPEC 9.2).
#![allow(unused_variables)] // P00 stub: remove when implemented.

/// For each line of `child`, the matching line index in `parent` (via `diff_lines`), or `None`
/// for lines introduced by the child.
pub fn line_origins(parent: &[u8], child: &[u8]) -> Vec<Option<usize>> {
    todo!("P02")
}
