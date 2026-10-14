//! `.scion/lock` advisory lock (SPEC 11.1; P11).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use scion_core::Result;
use std::path::Path;

/// A held working-copy lock; released on drop.
pub struct WcLock {
    _file: std::fs::File,
}

/// Take the fs4 advisory lock without blocking (error when held elsewhere).
pub fn acquire(scion_dir: &Path) -> Result<WcLock> {
    todo!("P11")
}
