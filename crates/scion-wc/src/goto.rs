//! SPEC 11.3 goto (P18).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::wc::WorkingCopy;
use scion_core::Result;

/// Result of a goto (`--json` schema `scion.cli.goto.v1`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct GotoReport {
    /// Previous base (id text).
    pub from: String,
    /// New base (id text).
    pub to: String,
    /// Files written.
    pub written: u64,
    /// Files removed.
    pub removed: u64,
    /// Dirty files preserved untouched.
    pub preserved_dirty: Vec<String>,
}

impl WorkingCopy {
    /// Move to `rev`, applying only the pruned diff within scope; dirty files the diff would
    /// touch abort unless `force`.
    pub async fn goto(&mut self, rev: &str, force: bool) -> Result<GotoReport> {
        todo!("P18")
    }
}
