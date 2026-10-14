//! Hot-reloadable policy handle (SPEC 6.2 reload semantics; P05).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::error::PolicyError;
use crate::policy::Policy;
use std::path::Path;
use std::sync::Arc;

/// arc-swap of the current policy plus a notify watcher (with a 1 s polling fallback).
pub struct PolicyHandle {
    current: arc_swap::ArcSwap<Policy>,
}

impl PolicyHandle {
    /// Load from a file; an invalid or missing file is an error (startup fails).
    pub fn load(path: &Path) -> Result<Arc<Self>, PolicyError> {
        todo!("P05")
    }

    /// A handle over a fixed policy (no file).
    pub fn from_policy(p: Policy) -> Arc<Self> {
        Arc::new(PolicyHandle {
            current: arc_swap::ArcSwap::from_pointee(p),
        })
    }

    /// The current policy snapshot.
    pub fn current(&self) -> Arc<Policy> {
        self.current.load_full()
    }

    /// The current policy epoch.
    pub fn epoch(&self) -> u64 {
        self.current.load().epoch()
    }

    /// Re-read the file; swaps only a valid policy. Returns whether the epoch changed. An
    /// invalid file keeps the last good policy and records `last_error`.
    pub fn reload_now(&self) -> Result<bool, PolicyError> {
        todo!("P05")
    }

    /// The last reload error, if the most recent reload failed.
    pub fn last_error(&self) -> Option<PolicyError> {
        todo!("P05")
    }

    /// Spawn the file watcher (notify plus 1 s polling fallback).
    pub fn spawn_watcher(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        todo!("P05")
    }

    /// Subscribe to epoch changes.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        todo!("P05")
    }
}
