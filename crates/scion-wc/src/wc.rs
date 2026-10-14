//! The [`WorkingCopy`] handle (SPEC 11; P18).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::state::WcState;
use crate::status::StatusReport;
use scion_core::{FaultRegistry, Result};
use scion_proto::ScionClient;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Working-copy options.
#[derive(Clone)]
pub struct WcOptions {
    /// Client fault points (SPEC 19.0).
    pub faults: Arc<FaultRegistry>,
    /// Treat the filesystem as case-insensitive (PLAN 6.17).
    pub force_case_insensitive: bool,
    /// Objects per GetObjects batch.
    pub fetch_batch: usize,
}

impl Default for WcOptions {
    fn default() -> Self {
        WcOptions {
            faults: FaultRegistry::new(),
            force_case_insensitive: false,
            fetch_batch: 256,
        }
    }
}

/// An opened working copy.
pub struct WorkingCopy {
    pub(crate) root: PathBuf,
    pub(crate) client: ScionClient,
    pub(crate) opts: WcOptions,
    pub(crate) state: WcState,
}

impl WorkingCopy {
    /// Open an existing working copy at `root`.
    pub fn open(root: &Path, client: ScionClient, opts: WcOptions) -> Result<Self> {
        todo!("P18")
    }

    /// The current state.
    pub fn state(&self) -> &WcState {
        &self.state
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// SPEC 11.5 status of the sparse scope (zero RPCs when trees are cached).
    pub async fn status(&mut self) -> Result<StatusReport> {
        todo!("P18")
    }
}
