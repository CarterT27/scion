//! Test scaling helpers (PLAN 3.5).

/// Number of property-test cases: `SCION_PROPTEST_CASES` overrides `default`. Spec-sized
/// counts live in `#[ignore] full_*` tests.
pub fn proptest_cases(default: u32) -> u32 {
    std::env::var("SCION_PROPTEST_CASES")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(default)
}

/// True when `SCION_FULL=1` (spec-sized runs via `scripts/check.sh --full`).
pub fn full_mode() -> bool {
    std::env::var("SCION_FULL")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_without_env() {
        if std::env::var_os("SCION_PROPTEST_CASES").is_none() {
            assert_eq!(proptest_cases(123), 123);
        }
        if std::env::var_os("SCION_FULL").is_none() {
            assert!(!full_mode());
        }
    }
}
