//! Constant-time comparison and redacting secret wrapper (SPEC 17; P06).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use std::fmt;

/// The single constant-time comparison primitive for secrets (wraps `subtle`).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    todo!("P06")
}

/// Number of [`ct_eq`] calls so far (feature `test-hooks`).
#[cfg(feature = "test-hooks")]
pub fn ct_eq_calls() -> u64 {
    todo!("P06")
}

/// A secret string whose `Debug`/`Display` print `***` (never logged, PLAN 6.9).
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
        assert_eq!(s.expose(), "hunter2");
    }
}
