//! Constant-time comparison and redacting secret wrapper (SPEC 12.3, 17; P06).

use std::fmt;
use subtle::ConstantTimeEq;

#[cfg(feature = "test-hooks")]
static CT_EQ_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The single constant-time comparison primitive for secrets (wraps `subtle`; SPEC 12.3, 19.11).
/// Inputs of different lengths compare unequal (the length is not secret: callers compare
/// fixed-size digests).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    #[cfg(feature = "test-hooks")]
    CT_EQ_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    bool::from(a.ct_eq(b))
}

/// Number of [`ct_eq`] calls so far in this process (feature `test-hooks`; SPEC 19.11).
#[cfg(feature = "test-hooks")]
pub fn ct_eq_calls() -> u64 {
    CT_EQ_CALLS.load(std::sync::atomic::Ordering::SeqCst)
}

/// A secret string whose `Debug`/`Display` print `***` (never logged; SPEC 12.3, PLAN 6.9).
pub struct Secret(String);

impl Secret {
    /// Wrap a secret.
    pub fn new(s: String) -> Self {
        Secret(s)
    }

    /// The secret value. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_is_redacted() {
        let s = Secret::new("hunter2".into());
        assert_eq!(format!("{s}"), "***");
        assert_eq!(format!("{s:?}"), "***");
        assert_eq!(format!("{:#?}", Some(&s)), "Some(\n    ***,\n)");
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn ct_eq_semantics() {
        assert!(ct_eq(b"", b""));
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }
}
