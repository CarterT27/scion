//! SPEC 15.2 Prometheus metrics plus `scion_integrity_failures_total` (PLAN 6.10; P04).

use prometheus::{
    Encoder, GaugeVec, Histogram, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts,
    Registry, TextEncoder,
};
use std::sync::Arc;

struct SnapshotCollector {
    values: IntGaugeVec,
    snapshot: Box<dyn Fn() -> Vec<(&'static str, u64)> + Send + Sync>,
}

impl prometheus::core::Collector for SnapshotCollector {
    fn desc(&self) -> Vec<&prometheus::core::Desc> {
        self.values.desc()
    }

    fn collect(&self) -> Vec<prometheus::proto::MetricFamily> {
        for (kind, value) in (self.snapshot)() {
            self.values
                .with_label_values(&[kind])
                .set(i64::try_from(value).unwrap_or(i64::MAX));
        }
        self.values.collect()
    }
}

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

/// Unwrap a metric constructor or registration result.
///
/// Every metric below has a constant, valid name, help text and label set, and is registered
/// exactly once in a fresh registry, so these calls cannot fail.
#[allow(clippy::expect_used)] // SAFETY-ish: constant metric definitions in a fresh registry.
fn ok<T>(r: prometheus::Result<T>) -> T {
    r.expect("constant Prometheus metric definition")
}

fn register<C: prometheus::core::Collector + Clone + 'static>(registry: &Registry, c: C) -> C {
    ok(registry.register(Box::new(c.clone())));
    c
}

/// Buckets (seconds) for RPC, land and derive durations: 1 ms .. 60 s.
const DURATION_BUCKETS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

fn histogram_vec(r: &Registry, name: &str, help: &str, labels: &[&str]) -> HistogramVec {
    let opts = HistogramOpts::new(name, help).buckets(DURATION_BUCKETS.to_vec());
    register(r, ok(HistogramVec::new(opts, labels)))
}

fn counter_vec(r: &Registry, name: &str, help: &str, labels: &[&str]) -> IntCounterVec {
    register(r, ok(IntCounterVec::new(Opts::new(name, help), labels)))
}

fn int_gauge_vec(r: &Registry, name: &str, help: &str, labels: &[&str]) -> IntGaugeVec {
    register(r, ok(IntGaugeVec::new(Opts::new(name, help), labels)))
}

impl Metrics {
    /// Export an existing set of counters at scrape time, without polling the backend.
    /// Values are gauges because their source may be reset independently of this registry.
    pub fn register_snapshot(
        &self,
        name: &str,
        help: &str,
        snapshot: impl Fn() -> Vec<(&'static str, u64)> + Send + Sync + 'static,
    ) -> prometheus::Result<()> {
        self.registry.register(Box::new(SnapshotCollector {
            values: IntGaugeVec::new(Opts::new(name, help), &["kind"])?,
            snapshot: Box::new(snapshot),
        }))
    }

    /// Built-in exported metric names; callers may register additional snapshot metrics.
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

    /// Create and register every metric (SPEC 15.2, PLAN 6.10) in a fresh registry.
    pub fn new() -> Arc<Self> {
        let r = Registry::new();
        let rpc_duration = histogram_vec(
            &r,
            "scion_rpc_duration_seconds",
            "RPC latency in seconds by rpc, tier and status code.",
            &["rpc", "tier", "code"],
        );
        let cache_requests = counter_vec(
            &r,
            "scion_cache_requests_total",
            "Cache lookups by tier, kind and result (hit|miss).",
            &["tier", "kind", "result"],
        );
        let cache_bytes = int_gauge_vec(
            &r,
            "scion_cache_bytes",
            "Bytes held in the cache by tier.",
            &["tier"],
        );
        let land_attempts = counter_vec(
            &r,
            "scion_land_attempts",
            "Land attempts by result.",
            &["result"],
        );
        let ref_cas_failures = counter_vec(
            &r,
            "scion_ref_cas_failures_total",
            "Ref compare-and-swap failures by ref.",
            &["ref"],
        );
        let land_duration = register(
            &r,
            ok(Histogram::with_opts(
                HistogramOpts::new(
                    "scion_land_duration_seconds",
                    "Land latency in seconds, all attempts included.",
                )
                .buckets(DURATION_BUCKETS.to_vec()),
            )),
        );
        let derive_duration = histogram_vec(
            &r,
            "scion_derive_duration_seconds",
            "Derivation latency in seconds by derived type and mode (eager|lazy).",
            &["type", "mode"],
        );
        let derive_backlog = int_gauge_vec(
            &r,
            "scion_derive_backlog",
            "Pending lazy derivations by derived type.",
            &["type"],
        );
        let redaction_epoch = int_gauge_vec(
            &r,
            "scion_redaction_epoch",
            "Current redaction epoch by tier.",
            &["tier"],
        );
        let redaction_lag = register(
            &r,
            ok(GaugeVec::new(
                Opts::new(
                    "scion_redaction_lag_seconds",
                    "Seconds since the redaction set was last refreshed, by tier.",
                ),
                &["tier"],
            )),
        );
        let policy_epoch_info = int_gauge_vec(
            &r,
            "scion_policy_epoch_info",
            "Info metric: 1 for the loaded policy epoch.",
            &["epoch"],
        );
        let object_bytes_served = counter_vec(
            &r,
            "scion_object_bytes_served_total",
            "Object bytes served by tier and encoding (raw|zstd).",
            &["tier", "encoding"],
        );
        let upstream_requests = counter_vec(
            &r,
            "scion_upstream_requests_total",
            "Requests sent upstream by tier.",
            &["tier"],
        );
        let integrity_failures = counter_vec(
            &r,
            "scion_integrity_failures_total",
            "Corrupt stored copies detected by tier (SPEC 16.1 class 7 alert).",
            &["tier"],
        );
        Arc::new(Metrics {
            registry: r,
            rpc_duration,
            cache_requests,
            cache_bytes,
            land_attempts,
            ref_cas_failures,
            land_duration,
            derive_duration,
            derive_backlog,
            redaction_epoch,
            redaction_lag,
            policy_epoch_info,
            object_bytes_served,
            upstream_requests,
            integrity_failures,
        })
    }

    /// Prometheus text exposition of the registry (SPEC 15.2).
    pub fn render(&self) -> String {
        let mut out = Vec::new();
        if let Err(e) = TextEncoder::new().encode(&self.registry.gather(), &mut out) {
            tracing::error!(target: "scion::obs", error = %e, "metrics encoding failed");
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Record the SPEC 16.1 class 7 alert (PLAN 6.10): increment
    /// `scion_integrity_failures_total{tier}` and emit an ERROR event with
    /// `event="integrity_alert"` and the object id. The caller returns INTEGRITY.
    pub fn integrity_alert(&self, tier: &str, object: &scion_core::ObjectId) {
        self.integrity_failures.with_label_values(&[tier]).inc();
        tracing::error!(
            target: "scion::integrity",
            event = "integrity_alert",
            tier = tier,
            object = %object,
            "stored copy failed digest verification"
        );
    }

    /// Set `scion_policy_epoch_info{epoch}` to 1 for `epoch` and drop the previous epoch's
    /// series (SPEC 15.2 info metric).
    pub fn set_policy_epoch(&self, epoch: u64) {
        self.policy_epoch_info.reset();
        self.policy_epoch_info
            .with_label_values(&[&epoch.to_string()])
            .set(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_are_current_and_do_not_accumulate_between_scrapes() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let m = Metrics::new();
        let value = Arc::new(AtomicU64::new(3));
        let observed = value.clone();
        m.register_snapshot("test_storage_access", "Test snapshot.", move || {
            vec![("reads", observed.load(Ordering::Relaxed))]
        })
        .unwrap();
        assert!(m.render().contains("test_storage_access{kind=\"reads\"} 3"));
        value.store(8, Ordering::Relaxed);
        assert!(m.render().contains("test_storage_access{kind=\"reads\"} 8"));
        value.store(0, Ordering::Relaxed);
        assert!(m.render().contains("test_storage_access{kind=\"reads\"} 0"));
    }

    #[test]
    fn registers_every_name() {
        let m = Metrics::new();
        let families = m.registry.gather();
        // Only unlabelled metrics appear before any observation.
        assert!(
            families
                .iter()
                .any(|f| f.name() == "scion_land_duration_seconds")
        );
        m.set_policy_epoch(7);
        m.set_policy_epoch(9);
        let text = m.render();
        assert!(text.contains("scion_policy_epoch_info{epoch=\"9\"} 1"));
        assert!(!text.contains("epoch=\"7\""));
    }

    #[test]
    fn integrity_alert_counts() {
        let m = Metrics::new();
        let id = scion_core::ObjectId::hash_blob(b"x");
        m.integrity_alert("server", &id);
        m.integrity_alert("server", &id);
        assert_eq!(m.integrity_failures.with_label_values(&["server"]).get(), 2);
    }
}
