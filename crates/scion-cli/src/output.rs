//! `--json` output envelope and published schemas (PLAN 6.15; P31).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use std::path::{Path, PathBuf};

/// Version of every published `--json` schema.
pub const SCHEMA_VERSION: u32 = 1;

/// `{"schema":"scion.cli.<cmd>.v1","data":...}`.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct Envelope<T> {
    /// `scion.cli.<cmd>.v1`.
    pub schema: String,
    /// Command-specific payload.
    pub data: T,
}

/// Write `schemas/v1/<cmd>.schema.json` for every command; returns the paths.
pub fn write_schemas(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    todo!("P31")
}

/// Every command with a `--json` schema.
pub fn command_names() -> &'static [&'static str] {
    todo!("P31")
}
