//! Redaction configuration and outcomes (SPEC 6.1 `[redaction]`).

use std::time::{Duration, SystemTime};

/// Redaction configuration.
#[derive(Clone, Debug)]
pub struct RedactionConfig {
    /// Propagation bound (default 1 s; SPEC 13.3).
    pub propagation_bound: Duration,
    /// Default quarantine before purge (default 7 d).
    pub default_quarantine: Duration,
    /// How often the purge scheduler runs.
    pub purge_interval: Duration,
}

impl Default for RedactionConfig {
    fn default() -> Self {
        RedactionConfig {
            propagation_bound: Duration::from_millis(1000),
            default_quarantine: Duration::from_secs(7 * 24 * 3600),
            purge_interval: Duration::from_secs(60),
        }
    }
}

/// Result of a redaction (SPEC 7.7 `{epoch, effective_by}`).
#[derive(Clone, Debug, PartialEq)]
pub struct RedactOutcome {
    /// The new redaction epoch.
    pub epoch: u64,
    /// `now + propagation_bound`.
    pub effective_by: SystemTime,
}
