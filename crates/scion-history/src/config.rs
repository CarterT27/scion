//! History configuration (SPEC 6.1 `[limits]`, `[land]`; PLAN 2.6).

use scion_core::{Limits, Principal};
use std::time::Duration;

/// Land retry configuration (SPEC 8.4, 6.1 `[land]`).
#[derive(Clone, Debug)]
pub struct LandConfig {
    /// Maximum attempts before UNAVAILABLE/land_contention (default 32).
    pub max_attempts: u32,
    /// Full-jitter backoff lower bound (default 5 ms).
    pub backoff_base: Duration,
    /// Full-jitter backoff upper bound (default 500 ms).
    pub backoff_max: Duration,
    /// Seed for the backoff RNG (tests); `None` seeds from the clock.
    pub rng_seed: Option<u64>,
}

impl Default for LandConfig {
    fn default() -> Self {
        LandConfig {
            max_attempts: 32,
            backoff_base: Duration::from_millis(5),
            backoff_max: Duration::from_millis(500),
            rng_seed: None,
        }
    }
}

/// History engine configuration.
#[derive(Clone, Debug)]
pub struct HistoryConfig {
    /// SPEC 6.5 limits.
    pub limits: Limits,
    /// Land retry configuration.
    pub land: LandConfig,
    /// Creator of the bootstrap root changeset (`service:scion`, PLAN 6.5).
    pub bootstrap_principal: Principal,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        HistoryConfig {
            limits: Limits::default(),
            land: LandConfig::default(),
            bootstrap_principal: Principal::Service("scion".to_owned()),
        }
    }
}
