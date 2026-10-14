//! SPEC 7.1 epoch trailers on every response (P08).

use scion_core::Tier;
use std::sync::Arc;

/// Trailer carrying the policy epoch as a decimal u64 (SPEC 7.1, PLAN 6.2).
pub const TRAILER_POLICY_EPOCH: &str = "scion-policy-epoch";
/// Trailer carrying the redaction epoch as a decimal u64 (SPEC 7.1, PLAN 6.2).
pub const TRAILER_REDACTION_EPOCH: &str = "scion-redaction-epoch";
/// Trailer carrying the serving tier, `server` or `daemon` (SPEC 7.1).
pub const TRAILER_TIER: &str = "scion-tier";

/// The three trailer values (SPEC 7.1).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Epochs {
    /// `scion-policy-epoch`.
    pub policy: u64,
    /// `scion-redaction-epoch`.
    pub redaction: u64,
    /// `scion-tier`.
    pub tier: Tier,
}

/// Supplies the current epochs for [`EpochTrailerLayer`].
pub trait EpochSource: Send + Sync + 'static {
    /// The epochs to stamp on the response being completed.
    fn epochs(&self) -> Epochs;
}

/// Parse the three epoch keys from a header or trailer map; `None` unless all are valid.
pub fn read_epochs(h: &http::HeaderMap) -> Option<Epochs> {
    let _ = h;
    todo!("P08")
}

/// Write the three epoch keys into a header or trailer map.
pub fn write_epochs(h: &mut http::HeaderMap, e: &Epochs) {
    let _ = (h, e);
    todo!("P08")
}

/// A tower `Layer` over HTTP bodies that appends the three trailers to the gRPC trailers frame of
/// EVERY response: unary, streaming, error and trailers-only (SPEC 7.1).
#[derive(Clone)]
pub struct EpochTrailerLayer {
    #[allow(dead_code)]
    src: Arc<dyn EpochSource>,
}

impl EpochTrailerLayer {
    /// A layer stamping epochs from `src`.
    pub fn new(src: Arc<dyn EpochSource>) -> Self {
        EpochTrailerLayer { src }
    }
}
