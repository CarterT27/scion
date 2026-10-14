//! Line splitting (SPEC 8.3, line-diff3-v1 section 1).

/// Split on `\n` keeping terminators; a final unterminated line is a line; empty input has no
/// lines.
///
/// SPEC 8.3 content merge; `docs/contracts/line-diff3-v1.md` section 1. No byte other than
/// `0x0A` is special: `\r` is ordinary and no CRLF normalization happens.
pub fn split_lines(b: &[u8]) -> Vec<&[u8]> {
    b.split_inclusive(|&c| c == b'\n').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_section_1_examples() {
        assert!(split_lines(b"").is_empty());
        assert_eq!(split_lines(b"\n"), vec![&b"\n"[..]]);
        assert_eq!(split_lines(b"a"), vec![&b"a"[..]]);
        assert_eq!(split_lines(b"a\n"), vec![&b"a\n"[..]]);
        assert_eq!(split_lines(b"a\nb"), vec![&b"a\n"[..], &b"b"[..]]);
        assert_eq!(split_lines(b"\n\n"), vec![&b"\n"[..], &b"\n"[..]]);
        assert_eq!(split_lines(b"x\r\ny\n"), vec![&b"x\r\n"[..], &b"y\n"[..]]);
    }

    #[test]
    fn concatenation_round_trips() {
        for input in [&b""[..], b"a", b"a\n", b"\r\n\r", b"a\nb\n\nc"] {
            assert_eq!(split_lines(input).concat(), input);
        }
    }
}
