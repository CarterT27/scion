//! Conflict marker rendering for working copies (SPEC 11.7). Never used for server objects.
#![allow(unused_variables)] // P00 stub: remove when implemented.

/// Render markers `<<<<<<< ours`, `||||||| base`, `=======`, `>>>>>>> theirs` around the
/// conflicting regions of a content merge.
pub fn render_markers(base: &[u8], ours: &[u8], theirs: &[u8]) -> Vec<u8> {
    todo!("P02")
}
