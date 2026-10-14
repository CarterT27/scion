//! SPEC 11.7 rebase with conflict materialization (P28).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use crate::wc::WorkingCopy;
use scion_core::Result;

/// Result of a rebase (`--json` schema `scion.cli.rebase.v1`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct RebaseReport {
    /// Destination (id text).
    pub onto: String,
    /// Rebased results (id text), in stack order.
    pub rebased: Vec<String>,
    /// Conflicting paths (lossy UTF-8), empty when clean.
    pub conflicts: Vec<String>,
    /// New base (id text) when the rebase completed.
    pub head: Option<String>,
}

impl WorkingCopy {
    /// Rebase the local stack onto `onto`; conflicts are materialized and recorded in pending.
    pub async fn rebase(&mut self, onto: &str) -> Result<RebaseReport> {
        todo!("P28")
    }

    /// Return to the original changeset and clear pending.
    pub async fn rebase_abort(&mut self) -> Result<()> {
        todo!("P28")
    }
}

/// Resume or roll back a pending Rebase (called by `WorkingCopy::recover`).
pub(crate) async fn recover_rebase(wc: &mut WorkingCopy) -> Result<Option<String>> {
    todo!("P28")
}
