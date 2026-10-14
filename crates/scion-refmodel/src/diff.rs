//! The model's own Myers and diff3, written from `docs/contracts/line-diff3-v1.md`.
#![allow(unused_variables)] // P00 stub: remove when implemented.

/// Edit script as `(op, a_index, b_index)` with op `'='`, `'-'` or `'+'`, one entry per line.
pub fn myers(a: &[&[u8]], b: &[&[u8]]) -> Vec<(char, usize, usize)> {
    todo!("P07")
}

/// Split on `\n` keeping terminators.
pub fn split_lines(b: &[u8]) -> Vec<&[u8]> {
    todo!("P07")
}

/// line-diff3-v1 merge: merged bytes, or conflicting base line ranges `(start, end)`.
pub fn merge3(b: &[u8], o: &[u8], t: &[u8]) -> Result<Vec<u8>, Vec<(usize, usize)>> {
    todo!("P07")
}
