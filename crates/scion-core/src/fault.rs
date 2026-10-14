//! SPEC 19.0 Fault Hooks: named injection points that are inert unless armed.
//!
//! Production code calls [`FaultRegistry::check`] at named points (see [`points`]); storage
//! decorators add `meta.<method>.before|after` and `blob.<method>.before|after`. When nothing is
//! armed and no crash is latched, the decision is a single relaxed atomic load (the per-point hit
//! counter is still incremented so tests can assert that a point was reached).

use crate::error::{ErrorCode, ErrorPayload, Result, ScionError};
use crate::subcode;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// What an armed point does when checked.
#[derive(Clone, Debug)]
pub enum FaultAction {
    /// Return `ScionError(code)` with subcode `injected` before the effect.
    Error(ErrorCode),
    /// Return UNAVAILABLE/`injected_crash` and latch every later check on this registry into
    /// failure until [`FaultRegistry::reset`] (models a dead process).
    Crash,
    /// Sleep for the duration, then continue.
    Delay(Duration),
    /// Decorators perform the effect, then return UNAVAILABLE/`injected` (models "commit
    /// happened, response lost"). Consumed through [`FaultRegistry::armed_after`]; a plain
    /// [`FaultRegistry::check`] of such a point is a no-op.
    FailAfter,
}

#[derive(Debug)]
struct Armed {
    action: FaultAction,
    remaining: Option<u32>,
}

/// A registry of armed fault points (SPEC 19.0).
#[derive(Debug, Default)]
pub struct FaultRegistry {
    active: AtomicBool,
    crashed: AtomicBool,
    armed: Mutex<HashMap<String, Armed>>,
    hits: RwLock<HashMap<String, AtomicU64>>,
}

fn injected(code: ErrorCode, point: &str) -> ScionError {
    ScionError::new(code, format!("injected fault at {point}"))
        .with_payload(ErrorPayload::Subcode(subcode::INJECTED.to_owned()))
}

fn crashed_error(point: &str) -> ScionError {
    ScionError::unavailable(
        subcode::INJECTED_CRASH,
        format!("injected crash at {point}"),
    )
}

impl FaultRegistry {
    /// A new, inert registry.
    pub fn new() -> Arc<Self> {
        Arc::new(FaultRegistry::default())
    }

    /// Arm `point` with `action` for `times` firings (`None` = always). Re-arming replaces.
    pub fn arm(&self, point: &str, action: FaultAction, times: Option<u32>) {
        let mut armed = self.armed.lock();
        armed.insert(
            point.to_owned(),
            Armed {
                action,
                remaining: times,
            },
        );
        self.active.store(true, Ordering::SeqCst);
    }

    /// Disarm every point (a latched crash stays latched).
    pub fn disarm_all(&self) {
        let mut armed = self.armed.lock();
        armed.clear();
        self.active
            .store(self.crashed.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    /// Disarm every point and clear the crash latch. Hit counters are kept.
    pub fn reset(&self) {
        let mut armed = self.armed.lock();
        armed.clear();
        self.crashed.store(false, Ordering::SeqCst);
        self.active.store(false, Ordering::SeqCst);
    }

    /// Clear all hit counters.
    pub fn reset_hits(&self) {
        self.hits.write().clear();
    }

    /// Number of times `point` was checked, armed or not.
    pub fn hits(&self, point: &str) -> u64 {
        self.hits
            .read()
            .get(point)
            .map_or(0, |c| c.load(Ordering::SeqCst))
    }

    /// True while a crash is latched.
    pub fn is_crashed(&self) -> bool {
        self.crashed.load(Ordering::SeqCst)
    }

    fn record_hit(&self, point: &str) {
        if let Some(c) = self.hits.read().get(point) {
            c.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.hits
            .write()
            .entry(point.to_owned())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Take one firing of the action armed at `point`, if any.
    fn take(&self, point: &str, want_fail_after: bool) -> Option<FaultAction> {
        let mut armed = self.armed.lock();
        let entry = armed.get_mut(point)?;
        let is_fail_after = matches!(entry.action, FaultAction::FailAfter);
        if is_fail_after != want_fail_after {
            return None;
        }
        match entry.remaining {
            Some(0) => return None,
            Some(ref mut n) => *n -= 1,
            None => {}
        }
        Some(entry.action.clone())
    }

    /// Check a named point. Inert (Ok) unless armed or a crash is latched.
    pub async fn check(&self, point: &str) -> Result<()> {
        self.record_hit(point);
        if !self.active.load(Ordering::Relaxed) {
            return Ok(());
        }
        if self.crashed.load(Ordering::SeqCst) {
            return Err(crashed_error(point));
        }
        match self.take(point, false) {
            None | Some(FaultAction::FailAfter) => Ok(()),
            Some(FaultAction::Error(code)) => Err(injected(code, point)),
            Some(FaultAction::Crash) => {
                self.crashed.store(true, Ordering::SeqCst);
                Err(crashed_error(point))
            }
            Some(FaultAction::Delay(d)) => {
                tokio::time::sleep(d).await;
                Ok(())
            }
        }
    }

    /// For decorators: true when `point` is armed with [`FaultAction::FailAfter`] (consuming
    /// one firing) or a crash is latched. The caller then returns UNAVAILABLE/`injected`
    /// after performing the effect.
    pub fn armed_after(&self, point: &str) -> bool {
        if !self.active.load(Ordering::Relaxed) {
            return false;
        }
        if self.crashed.load(Ordering::SeqCst) {
            return true;
        }
        self.take(point, true).is_some()
    }

    /// The error decorators return for a [`FaultAction::FailAfter`] firing.
    pub fn fail_after_error(point: &str) -> ScionError {
        injected(ErrorCode::Unavailable, point)
    }
}

/// Checkpoint names between the SPEC 5.3 steps, land, redaction and client operations.
/// Storage decorators add `meta.<method>.before|after` and `blob.<method>.before|after`.
pub mod points {
    /// SPEC 5.3 step 1: object bytes written, row not yet inserted.
    pub const UPLOAD_AFTER_BYTES: &str = "upload.after_bytes";
    /// SPEC 5.3 step 1: objects row inserted.
    pub const UPLOAD_AFTER_ROW: &str = "upload.after_row";
    /// SPEC 5.3 step 2: validation finished.
    pub const CREATE_AFTER_VALIDATE: &str = "create.after_validate";
    /// SPEC 5.3 step 2: new trees written.
    pub const CREATE_AFTER_TREES: &str = "create.after_trees";
    /// SPEC 5.3 step 2: Meta written.
    pub const CREATE_AFTER_META: &str = "create.after_meta";
    /// SPEC 5.3 step 2: eager derived values computed.
    pub const CREATE_AFTER_DERIVE: &str = "create.after_derive";
    /// SPEC 5.3 step 2: changeset transaction committed.
    pub const CREATE_AFTER_COMMIT: &str = "create.after_commit";
    /// SPEC 8.4: stack rebased, before commit.
    pub const LAND_AFTER_REBASE: &str = "land.after_rebase";
    /// SPEC 8.4: ref CAS committed.
    pub const LAND_AFTER_COMMIT: &str = "land.after_commit";
    /// SPEC 13.2: redaction transaction committed.
    pub const REDACT_AFTER_COMMIT: &str = "redact.after_commit";
    /// SPEC 13.2: bytes copied to the quarantine key, live key not yet deleted.
    pub const QUARANTINE_AFTER_COPY: &str = "quarantine.after_copy";
    /// SPEC 13.2: purged bytes deleted, rows not yet marked.
    pub const PURGE_AFTER_BYTES: &str = "purge.after_bytes";

    /// Every checkpoint name above.
    pub const ALL: &[&str] = &[
        UPLOAD_AFTER_BYTES,
        UPLOAD_AFTER_ROW,
        CREATE_AFTER_VALIDATE,
        CREATE_AFTER_TREES,
        CREATE_AFTER_META,
        CREATE_AFTER_DERIVE,
        CREATE_AFTER_COMMIT,
        LAND_AFTER_REBASE,
        LAND_AFTER_COMMIT,
        REDACT_AFTER_COMMIT,
        QUARANTINE_AFTER_COPY,
        PURGE_AFTER_BYTES,
    ];

    /// The decorator point name `meta.<method>.before`.
    pub fn meta_before(method: &str) -> String {
        format!("meta.{method}.before")
    }

    /// The decorator point name `meta.<method>.after`.
    pub fn meta_after(method: &str) -> String {
        format!("meta.{method}.after")
    }

    /// The decorator point name `blob.<method>.before`.
    pub fn blob_before(method: &str) -> String {
        format!("blob.{method}.before")
    }

    /// The decorator point name `blob.<method>.after`.
    pub fn blob_after(method: &str) -> String {
        format!("blob.{method}.after")
    }
}

/// Tracing conventions (SPEC 19.5 lock-free proof, PLAN 7.2).
pub mod trace {
    /// Every lease/lock acquisition emits a span with this target.
    pub const LOCK_TARGET: &str = "scion::lock";
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn unarmed_is_inert_and_counts_hits() {
        let f = FaultRegistry::new();
        assert_eq!(f.hits(points::CREATE_AFTER_META), 0);
        for _ in 0..3 {
            f.check(points::CREATE_AFTER_META).await.unwrap();
        }
        assert_eq!(f.hits(points::CREATE_AFTER_META), 3);
        assert_eq!(f.hits(points::CREATE_AFTER_TREES), 0);
        assert!(!f.armed_after("meta.cas_ref.after"));
    }

    #[tokio::test]
    async fn error_with_times() {
        let f = FaultRegistry::new();
        f.arm(
            points::UPLOAD_AFTER_ROW,
            FaultAction::Error(ErrorCode::Unavailable),
            Some(2),
        );
        for _ in 0..2 {
            let e = f.check(points::UPLOAD_AFTER_ROW).await.unwrap_err();
            assert_eq!(e.code, ErrorCode::Unavailable);
            assert_eq!(e.subcode(), Some(subcode::INJECTED));
        }
        f.check(points::UPLOAD_AFTER_ROW).await.unwrap();
        f.check(points::UPLOAD_AFTER_BYTES).await.unwrap();
        assert_eq!(f.hits(points::UPLOAD_AFTER_ROW), 3);
    }

    #[tokio::test]
    async fn always_until_disarmed() {
        let f = FaultRegistry::new();
        f.arm("p", FaultAction::Error(ErrorCode::NotFound), None);
        for _ in 0..5 {
            assert_eq!(f.check("p").await.unwrap_err().code, ErrorCode::NotFound);
        }
        f.disarm_all();
        f.check("p").await.unwrap();
    }

    #[tokio::test]
    async fn crash_latches_until_reset() {
        let f = FaultRegistry::new();
        f.arm(points::LAND_AFTER_COMMIT, FaultAction::Crash, Some(1));
        let e = f.check(points::LAND_AFTER_COMMIT).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Unavailable);
        assert_eq!(e.subcode(), Some(subcode::INJECTED_CRASH));
        assert!(f.is_crashed());
        // Every later check, at any point, fails.
        let e = f.check("anything.else").await.unwrap_err();
        assert_eq!(e.subcode(), Some(subcode::INJECTED_CRASH));
        assert!(f.armed_after("meta.x.after"));
        f.disarm_all();
        assert!(f.check("anything.else").await.is_err());
        f.reset();
        assert!(!f.is_crashed());
        f.check("anything.else").await.unwrap();
        f.check(points::LAND_AFTER_COMMIT).await.unwrap();
    }

    #[tokio::test]
    async fn delay() {
        let f = FaultRegistry::new();
        f.arm(
            "slow",
            FaultAction::Delay(Duration::from_millis(30)),
            Some(1),
        );
        let t = Instant::now();
        f.check("slow").await.unwrap();
        assert!(t.elapsed() >= Duration::from_millis(30));
        let t = Instant::now();
        f.check("slow").await.unwrap();
        assert!(t.elapsed() < Duration::from_millis(30));
    }

    #[tokio::test]
    async fn fail_after() {
        let f = FaultRegistry::new();
        let p = points::meta_after("cas_ref");
        assert_eq!(p, "meta.cas_ref.after");
        f.arm(&p, FaultAction::FailAfter, Some(1));
        // A plain check is a no-op for FailAfter.
        f.check(&p).await.unwrap();
        assert!(f.armed_after(&p));
        assert!(!f.armed_after(&p));
        let e = FaultRegistry::fail_after_error(&p);
        assert_eq!(e.code, ErrorCode::Unavailable);
        assert_eq!(e.subcode(), Some(subcode::INJECTED));
        // armed_after does not fire for non-FailAfter actions.
        f.arm("q", FaultAction::Error(ErrorCode::Unavailable), None);
        assert!(!f.armed_after("q"));
    }

    #[test]
    fn point_names() {
        assert_eq!(points::ALL.len(), 12);
        assert_eq!(points::blob_before("put"), "blob.put.before");
        assert_eq!(points::blob_after("put"), "blob.put.after");
        assert_eq!(points::meta_before("redact"), "meta.redact.before");
        assert_eq!(trace::LOCK_TARGET, "scion::lock");
    }
}
