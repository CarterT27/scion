//! Working-copy diff (P18).

use crate::wc::WorkingCopy;
use scion_core::Result;

/// One changed file (`--json` schema `scion.cli.diff.v1`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct FileChangeView {
    /// Path (lossy UTF-8 for display).
    pub path: String,
    /// `modified`, `added`, `deleted` or `mode_changed`.
    pub status: String,
    /// Old blob id text.
    pub old_id: Option<String>,
    /// New blob id text.
    pub new_id: Option<String>,
    /// Old mode (1-3).
    pub old_mode: Option<u8>,
    /// New mode (1-3).
    pub new_mode: Option<u8>,
}

impl WorkingCopy {
    /// Working-copy changes against the base.
    pub async fn diff(&mut self) -> Result<Vec<FileChangeView>> {
        todo!("P18")
    }
}
