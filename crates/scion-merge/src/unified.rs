//! Unified diff hunks for the Diff RPC (SPEC 7.4).
#![allow(unused_variables)] // P00 stub: remove when implemented.

/// One unified-diff hunk (1-based starts as in `@@ -a,b +c,d @@`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextHunk {
    /// Start line in the old text.
    pub old_start: u32,
    /// Line count in the old text.
    pub old_lines: u32,
    /// Start line in the new text.
    pub new_start: u32,
    /// Line count in the new text.
    pub new_lines: u32,
    /// Lines prefixed by `b' '`, `b'-'` or `b'+'`.
    pub lines: Vec<(u8, Vec<u8>)>,
}

/// Unified hunks between `a` and `b` with `context` lines of context.
pub fn unified_hunks(a: &[u8], b: &[u8], context: usize) -> Vec<TextHunk> {
    todo!("P02")
}
