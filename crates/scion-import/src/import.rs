//! The import walk (SPEC 14; P29).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use scion_core::{ObjectId, Result};
use scion_object::RefName;
use scion_proto::ScionClient;
use std::path::PathBuf;

/// Import options.
#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// The git repository.
    pub git_dir: PathBuf,
    /// The git ref to import.
    pub git_ref: String,
    /// The Scion ref to create or fast-forward.
    pub target_ref: RefName,
    /// Omit gitlinks (recorded in Meta extension tag 4) instead of failing.
    pub skip_submodules: bool,
    /// Upload batch size in bytes.
    pub batch_bytes: usize,
}

/// Import summary.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct ImportReport {
    /// Git commits reachable from the ref.
    pub commits_seen: u64,
    /// Changesets created.
    pub imported: u64,
    /// Commits already imported (incremental re-import).
    pub skipped_existing: u64,
    /// Blobs uploaded.
    pub blobs_uploaded: u64,
    /// Skipped gitlinks as `(path, sha hex)`.
    pub skipped_gitlinks: Vec<(String, String)>,
    /// The imported tip.
    pub final_target: Option<ObjectId>,
    /// Version of the target ref after the import.
    pub ref_version: u64,
}

/// Import `opts.git_ref` into `opts.target_ref` through `client`.
pub async fn import(client: &ScionClient, opts: &ImportOptions) -> Result<ImportReport> {
    todo!("P29")
}
