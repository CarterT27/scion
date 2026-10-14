//! `.scion/lock` advisory lock (SPEC 11.1; P11): one mutating command at a time.

use crate::fs_safe::io_error;
use scion_core::{ErrorCode, Result, ScionError};
use std::path::Path;

/// File name of the lock under `.scion/` (SPEC 11.1).
pub const LOCK_FILE: &str = "lock";

/// A held working-copy lock; released on drop (SPEC 11.1).
pub struct WcLock {
    _file: std::fs::File,
}

impl std::fmt::Debug for WcLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WcLock")
    }
}

/// Take the fs4 advisory lock on `<scion_dir>/lock` without blocking (SPEC 11.1). When another
/// holder (any process, or another handle in this process) has it, the error is UNAVAILABLE
/// (retryable).
pub fn acquire(scion_dir: &Path) -> Result<WcLock> {
    let _span = tracing::debug_span!(target: "scion::lock", "wc_lock_acquire").entered();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(scion_dir.join(LOCK_FILE))
        .map_err(|e| io_error("open .scion/lock", e))?;
    match fs4::fs_std::FileExt::try_lock_exclusive(&file) {
        Ok(true) => {
            tracing::debug!(target: "scion::lock", "working-copy lock acquired");
            Ok(WcLock { _file: file })
        }
        Ok(false) => Err(held()),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Err(held()),
        Err(e) => Err(io_error("lock .scion/lock", e)),
    }
}

fn held() -> ScionError {
    ScionError::new(
        ErrorCode::Unavailable,
        "the working copy is locked by another scion command; retry when it finishes",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_holder_is_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire(dir.path()).unwrap();
        let err = acquire(dir.path()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Unavailable);
        assert!(err.retryable());
        drop(first);
        let again = acquire(dir.path()).unwrap();
        drop(again);
    }

    #[test]
    fn missing_dir_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(acquire(&dir.path().join("nope")).is_err());
    }
}
