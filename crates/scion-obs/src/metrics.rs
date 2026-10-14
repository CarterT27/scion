//! SPEC 15.2 Prometheus metrics plus `scion_integrity_failures_total` (PLAN 6.10; P04).

use prometheus::{GaugeVec, Histogram, HistogramVec, IntCounterVec, IntGaugeVec, Registry};
use std::sync::Arc;

/// Every SPEC 15.2 metric, registered in one registry.
pub struct Metrics {
    /// The registry rendered at `/metrics`.
    pub registry: Registry,
    /// `scion_rpc_duration_seconds{rpc,tier,code}`.
    pub rpc_duration: HistogramVec,
    /// `scion_cache_requests_total{tier,kind,result}`.
    pub cache_requests: IntCounterVec,
    /// `scion_cache_bytes{tier}`.
    pub cache_bytes: IntGaugeVec,
    /// `scion_land_attempts{result}`.
    pub land_attempts: IntCounterVec,
    /// `scion_ref_cas_failures_total{ref}`.
    pub ref_cas_failures: IntCounterVec,
    /// `scion_land_duration_seconds`.
    pub land_duration: Histogram,
    /// `scion_derive_duration_seconds{type,mode}`.
    pub derive_duration: HistogramVec,
    /// `scion_derive_backlog{type}`.
    pub derive_backlog: IntGaugeVec,
    /// `scion_redaction_epoch{tier}`.
    pub redaction_epoch: IntGaugeVec,
    /// `scion_redaction_lag_seconds{tier}`.
    pub redaction_lag: GaugeVec,
    /// `scion_policy_epoch_info{epoch}`.
    pub policy_epoch_info: IntGaugeVec,
    /// `scion_object_bytes_served_total{tier,encoding}`.
    pub object_bytes_served: IntCounterVec,
    /// `scion_upstream_requests_total{tier}`.
    pub upstream_requests: IntCounterVec,
    /// (+) `scion_integrity_failures_total{tier}`: the SPEC 16.1 class 7 alert.
    pub integrity_failures: IntCounterVec,
}

impl Metrics {
    /// Every exported metric name.
    pub const NAMES: &'static [&'static str] = &[
        "scion_rpc_duration_seconds",
        "scion_cache_requests_total",
        "scion_cache_bytes",
        "scion_land_attempts",
        "scion_ref_cas_failures_total",
        "scion_land_duration_seconds",
        "scion_derive_duration_seconds",
        "scion_derive_backlog",
        "scion_redaction_epoch",
        "scion_redaction_lag_seconds",
        "scion_policy_epoch_info",
        "scion_object_bytes_served_total",
        "scion_upstream_requests_total",
        "scion_integrity_failures_total",
    ];

    /// Create and register every metric.
    pub fn new() -> Arc<Self> {
        todo!("P04")
    }

    /// Prometheus text exposition of the registry.
    pub fn render(&self) -> String {
        todo!("P04")
    }
}
