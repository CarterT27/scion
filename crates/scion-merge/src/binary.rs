//! Binary detection (SPEC 8.3).
#![allow(unused_variables)] // P00 stub: remove when implemented.

/// True when `b` contains `0x00` within its first 8000 bytes (git's heuristic).
pub fn is_binary(b: &[u8]) -> bool {
    todo!("P02")
}
