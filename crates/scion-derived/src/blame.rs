//! SPEC 9.2 `blame` (lazy, keyed by file history node id; not content-bearing; P24).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::engine::DerivedEngine;
use crate::types::{DeriveCtx, DeriveMode, DerivedType, LazyType};
use async_trait::async_trait;
use bytes::Bytes;
use scion_core::{ObjectId, Result};
use scion_object::RepoPath;

/// Type name.
pub const NAME: &str = "blame";

/// The lazy blame type; the version is configurable for version-bump tests.
#[derive(Clone, Debug)]
pub struct BlameType {
    version: u32,
}

impl BlameType {
    /// Blame at version 1.
    pub fn new() -> Self {
        BlameType { version: 1 }
    }

    /// Blame at version `v`.
    pub fn with_version(v: u32) -> Self {
        BlameType { version: v }
    }
}

impl Default for BlameType {
    fn default() -> Self {
        BlameType::new()
    }
}

impl DerivedType for BlameType {
    fn name(&self) -> &'static str {
        NAME
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn mode(&self) -> DeriveMode {
        DeriveMode::Lazy
    }

    fn content_bearing(&self) -> bool {
        false
    }
}

#[async_trait]
impl LazyType for BlameType {
    async fn deps(&self, cx: &dyn DeriveCtx, key: &[u8]) -> Result<Vec<Bytes>> {
        todo!("P24")
    }

    async fn derive(
        &self,
        cx: &dyn DeriveCtx,
        key: &[u8],
        deps: &[(Bytes, Bytes)],
    ) -> Result<Bytes> {
        todo!("P24")
    }
}

/// A run of lines attributed to one origin (SPEC 7.4 Blame).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameRange {
    /// First line (0-based) in the blamed file.
    pub start_line: u32,
    /// Number of lines.
    pub line_count: u32,
    /// Changeset that introduced the lines.
    pub origin_changeset: ObjectId,
    /// First line in the origin's version of the file.
    pub origin_start_line: u32,
}

/// Compact canonical encoding of ranges (adjacent equal-origin lines merged).
pub fn encode_ranges(r: &[BlameRange]) -> Bytes {
    todo!("P24")
}

/// Inverse of [`encode_ranges`].
pub fn decode_ranges(b: &[u8]) -> Result<Vec<BlameRange>> {
    todo!("P24")
}

/// Blame `path` at changeset `cs` (NOT_FOUND for a missing or non-file path).
pub async fn blame(
    e: &DerivedEngine,
    cs: &ObjectId,
    path: &RepoPath,
    version: Option<u32>,
) -> Result<Vec<BlameRange>> {
    todo!("P24")
}
