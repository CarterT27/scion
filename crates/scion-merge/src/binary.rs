//! Binary detection (SPEC 8.3).

// Number of leading bytes inspected by `is_binary` (SPEC 8.3, git's heuristic).
const BINARY_SNIFF_LEN: usize = 8000;

/// True when `b` contains `0x00` within its first 8000 bytes (git's heuristic).
///
/// SPEC 8.3 binary detection: a NUL at byte index 7999 makes the content binary; a NUL first
/// appearing at index 8000 or later does not.
pub fn is_binary(b: &[u8]) -> bool {
    let n = b.len().min(BINARY_SNIFF_LEN);
    b.get(..n).is_some_and(|head| head.contains(&0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary() {
        let mut v = vec![b'a'; 9000];
        assert!(!is_binary(&v));
        v[7999] = 0;
        assert!(is_binary(&v));
        v[7999] = b'a';
        v[8000] = 0;
        assert!(!is_binary(&v));
        assert!(!is_binary(b""));
        assert!(is_binary(b"\0"));
    }
}
