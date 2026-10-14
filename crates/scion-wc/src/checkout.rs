//! SPEC 11.3 checkout (P18).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::wc::{WcOptions, WorkingCopy};
use scion_core::Result;
use scion_object::RepoPath;
use scion_proto::ScionClient;
use std::path::Path;

impl WorkingCopy {
    /// Validate every path and case collision BEFORE any write, then materialize `rev` within
    /// `sparse` at `root`.
    pub async fn checkout(
        client: ScionClient,
        root: &Path,
        rev: &str,
        sparse: Vec<RepoPath>,
        opts: WcOptions,
    ) -> Result<Self> {
        todo!("P18")
    }
}
