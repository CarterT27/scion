//! Policy hot reload wiring (SPEC 6.2 reload semantics, 15.2, 16.1 class 9; P23).
//!
//! The [`scion_policy::PolicyHandle`] swaps only valid policies; an invalid file keeps the last
//! known good policy, records `last_error` and logs an operator-visible ERROR. This module keeps
//! `scion_policy_epoch_info{epoch}` in step with the handle and starts the file watcher when
//! `policy.reload` is set.

use scion_core::Result;
use scion_obs::Metrics;
use scion_policy::PolicyHandle;
use std::sync::Arc;

/// Track the policy epoch in `scion_policy_epoch_info` (SPEC 15.2). The task ends when the
/// handle is dropped.
pub fn spawn_epoch_metric(
    policy: &Arc<PolicyHandle>,
    metrics: Arc<Metrics>,
) -> tokio::task::JoinHandle<()> {
    let mut rx = policy.subscribe();
    metrics.set_policy_epoch(*rx.borrow_and_update());
    tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            let epoch = *rx.borrow_and_update();
            metrics.set_policy_epoch(epoch);
            tracing::info!(policy_epoch = epoch, "policy epoch changed");
        }
    })
}

/// Start the reload machinery: the epoch metric task always, the file watcher when
/// `reload` is set (SPEC 6.2). Returns the spawned tasks.
pub fn spawn(
    policy: &Arc<PolicyHandle>,
    metrics: Arc<Metrics>,
    reload: bool,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut tasks = vec![spawn_epoch_metric(policy, metrics)];
    if reload {
        tasks.push(policy.spawn_watcher());
    }
    tasks
}

/// Re-read the policy file now (SPEC 6.2). A valid change swaps atomically and moves the epoch
/// (the metric task follows); an invalid file keeps the last good policy, logs an ERROR (in
/// [`PolicyHandle`]) and is returned as INVALID_ARGUMENT. Returns whether the epoch changed.
pub fn reload_now(policy: &PolicyHandle, metrics: &Metrics) -> Result<bool> {
    match policy.reload_now() {
        Ok(changed) => {
            metrics.set_policy_epoch(policy.epoch());
            Ok(changed)
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const A: &str = "version = 1\n[[paths]]\nprefix = \"/\"\nread = [\"github:a\"]\nwrite = []\n";
    const B: &str = "version = 1\n[[paths]]\nprefix = \"/\"\nread = [\"github:b\"]\nwrite = []\n";

    #[tokio::test]
    async fn metric_follows_reloads_and_invalid_files_keep_last_good() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acl.toml");
        std::fs::write(&path, A).unwrap();
        let handle = PolicyHandle::load(&path).unwrap();
        let metrics = Metrics::new();
        let tasks = spawn(&handle, metrics.clone(), false);
        let e1 = handle.epoch();
        assert!(metrics.render().contains(&format!("epoch=\"{e1}\"")));

        std::fs::write(&path, B).unwrap();
        assert!(reload_now(&handle, &metrics).unwrap());
        let e2 = handle.epoch();
        assert_ne!(e1, e2);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !metrics.render().contains(&format!("epoch=\"{e2}\"")) {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        std::fs::write(&path, "version = 1\n[[paths]]\nprefix = \"nope\"\n").unwrap();
        let err = reload_now(&handle, &metrics).unwrap_err();
        assert_eq!(err.code, scion_core::ErrorCode::InvalidArgument);
        assert_eq!(handle.epoch(), e2);
        assert!(handle.last_error().is_some());
        assert!(metrics.render().contains(&format!("epoch=\"{e2}\"")));
        for t in tasks {
            t.abort();
        }
    }
}
