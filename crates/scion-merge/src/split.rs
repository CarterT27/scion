//! Line splitting (SPEC 8.3, line-diff3-v1 section 1).
#![allow(unused_variables)] // P00 stub: remove when implemented.

/// Split on `\n` keeping terminators; a final unterminated line is a line; empty input has no
/// lines.
pub fn split_lines(b: &[u8]) -> Vec<&[u8]> {
    todo!("P02")
}
