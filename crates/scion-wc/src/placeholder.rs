//! Redaction placeholders (SPEC 13.7; PLAN 6.15).

use scion_core::ObjectId;

/// Prefix of every placeholder file.
pub const REDACTED_PLACEHOLDER_PREFIX: &str = "Scion: contents redacted (object ";

/// Exactly `Scion: contents redacted (object <id text>).\n`.
pub fn placeholder_bytes(id: &ObjectId) -> Vec<u8> {
    format!("{REDACTED_PLACEHOLDER_PREFIX}{id}).\n").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::Kind;

    #[test]
    fn placeholder_format() {
        let id = ObjectId::new(Kind::Blob, [0xab; 32]);
        let expected = format!("Scion: contents redacted (object b1{}).\n", "ab".repeat(32));
        assert_eq!(placeholder_bytes(&id), expected.into_bytes());
    }
}
