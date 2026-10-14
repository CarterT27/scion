//! Hot-reloadable policy handle (SPEC 6.2 reload semantics, SPEC 16.1 class 9; P05).
//!
//! The current policy lives in an `ArcSwap`, so readers take a consistent snapshot with one
//! atomic load and never observe a partially applied reload. Reloads are serialized by a mutex;
//! a reload swaps only a valid policy. An invalid (or missing) file keeps the last known good
//! policy, records [`PolicyHandle::last_error`] and logs an operator-visible `ERROR`.

use crate::error::PolicyError;
use crate::policy::Policy;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

/// Polling fallback period for [`PolicyHandle::spawn_watcher`].
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Settle time after a filesystem event before re-reading (lets writers finish).
const EVENT_DEBOUNCE: Duration = Duration::from_millis(50);

/// arc-swap of the current policy plus a notify watcher (with a 1 s polling fallback).
pub struct PolicyHandle {
    current: arc_swap::ArcSwap<Policy>,
    /// The policy file; `None` for [`PolicyHandle::from_policy`].
    path: Option<PathBuf>,
    /// The error of the most recent reload, cleared by a successful one.
    last_error: Mutex<Option<PolicyError>>,
    /// Serializes reloads so swaps and epoch publication happen in order.
    reload_lock: Mutex<()>,
    /// Publishes the current epoch.
    epoch_tx: tokio::sync::watch::Sender<u64>,
}

fn read_file(path: &Path) -> Result<String, PolicyError> {
    let bytes =
        std::fs::read(path).map_err(|e| PolicyError::Io(format!("{}: {e}", path.display())))?;
    String::from_utf8(bytes).map_err(|_| PolicyError::Toml("policy file is not UTF-8".to_owned()))
}

impl PolicyHandle {
    fn build(p: Policy, path: Option<PathBuf>) -> Arc<Self> {
        let (epoch_tx, _) = tokio::sync::watch::channel(p.epoch());
        Arc::new(PolicyHandle {
            current: arc_swap::ArcSwap::from_pointee(p),
            path,
            last_error: Mutex::new(None),
            reload_lock: Mutex::new(()),
            epoch_tx,
        })
    }

    /// Load from a file; an invalid or missing file is an error (startup fails, SPEC 6.2).
    pub fn load(path: &Path) -> Result<Arc<Self>, PolicyError> {
        let text = read_file(path)?;
        let policy = Policy::parse(&text)?;
        tracing::info!(
            path = %path.display(),
            policy_epoch = policy.epoch(),
            "policy loaded"
        );
        Ok(Self::build(policy, Some(path.to_path_buf())))
    }

    /// A handle over a fixed policy (no file).
    pub fn from_policy(p: Policy) -> Arc<Self> {
        Self::build(p, None)
    }

    /// The current policy snapshot (SPEC 6.2).
    pub fn current(&self) -> Arc<Policy> {
        self.current.load_full()
    }

    /// The current policy epoch (SPEC 6.2, PLAN 6.2).
    pub fn epoch(&self) -> u64 {
        self.current.load().epoch()
    }

    /// Apply already-read file contents (or the read error). Serialized by `reload_lock`.
    fn apply(&self, text: Result<String, PolicyError>) -> Result<bool, PolicyError> {
        let _guard = self.reload_lock.lock();
        match text.and_then(|t| Policy::parse(&t)) {
            Ok(policy) => {
                *self.last_error.lock() = None;
                let old = self.epoch();
                let new = policy.epoch();
                if old == new {
                    return Ok(false);
                }
                self.current.store(Arc::new(policy));
                self.epoch_tx.send_replace(new);
                tracing::info!(old_epoch = old, policy_epoch = new, "policy reloaded");
                Ok(true)
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    path = %self.path.as_deref().map(|p| p.display().to_string()).unwrap_or_default(),
                    policy_epoch = self.epoch(),
                    "invalid policy reload; keeping last known good policy"
                );
                *self.last_error.lock() = Some(e.clone());
                Err(e)
            }
        }
    }

    /// Re-read the file; swaps only a valid policy. Returns whether the epoch changed. An
    /// invalid file keeps the last good policy and records `last_error` (SPEC 6.2, 16.1
    /// class 9). A handle without a file (`from_policy`) never changes: `Ok(false)`.
    pub fn reload_now(&self) -> Result<bool, PolicyError> {
        let Some(path) = self.path.as_deref() else {
            return Ok(false);
        };
        self.apply(read_file(path))
    }

    /// The last reload error, if the most recent reload failed (SPEC 16.1 class 9).
    pub fn last_error(&self) -> Option<PolicyError> {
        self.last_error.lock().clone()
    }

    /// Spawn the file watcher (notify plus 1 s polling fallback; SPEC 6.2 `policy.reload`).
    /// Must be called within a tokio runtime. The task ends when the last `Arc` of the handle
    /// is dropped, or when aborted through the returned handle. File contents are compared
    /// by digest, so an unchanged file is not re-applied (and an invalid one is reported once
    /// per distinct content).
    pub fn spawn_watcher(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(self);
        let path = self.path.clone();
        tokio::spawn(async move {
            let Some(path) = path else {
                return;
            };
            watch_loop(weak, path).await;
        })
    }

    /// Subscribe to epoch changes (SPEC 6.2).
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.epoch_tx.subscribe()
    }
}

/// What the watcher last saw on disk: the digest of the contents, or the read error text.
#[derive(PartialEq)]
enum Seen {
    Content([u8; 32]),
    Error(String),
}

async fn watch_loop(weak: Weak<PolicyHandle>, path: PathBuf) {
    use notify::Watcher;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    // Watch the parent directory: editors and config managers replace the file atomically.
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let _watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = tx.send(());
        }
    }) {
        Ok(mut w) => match w.watch(&dir, notify::RecursiveMode::NonRecursive) {
            Ok(()) => Some(w),
            Err(e) => {
                tracing::warn!(error = %e, "policy file watch failed; using polling only");
                None
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "policy watcher unavailable; using polling only");
            None
        }
    };

    // The file content at load time is what the handle already holds.
    let mut seen = match tokio::fs::read(&path).await {
        Ok(b) => Some(Seen::Content(*blake3::hash(&b).as_bytes())),
        Err(_) => None,
    };
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            ev = rx.recv() => {
                if ev.is_some() {
                    tokio::time::sleep(EVENT_DEBOUNCE).await;
                    while rx.try_recv().is_ok() {}
                }
            }
            _ = tick.tick() => {}
        }
        let Some(handle) = weak.upgrade() else {
            return;
        };
        let read = tokio::fs::read(&path).await;
        let now = match &read {
            Ok(b) => Seen::Content(*blake3::hash(b).as_bytes()),
            Err(e) => Seen::Error(e.kind().to_string()),
        };
        if seen.as_ref() == Some(&now) {
            continue;
        }
        seen = Some(now);
        let text = match read {
            Ok(b) => String::from_utf8(b)
                .map_err(|_| PolicyError::Toml("policy file is not UTF-8".to_owned())),
            Err(e) => Err(PolicyError::Io(format!("{}: {e}", path.display()))),
        };
        let h = Arc::clone(&handle);
        drop(handle);
        // Parsing is cheap but synchronous; keep it off the async worker.
        let _ = tokio::task::spawn_blocking(move || h.apply(text)).await;
    }
}
