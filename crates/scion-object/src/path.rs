//! SPEC 4.4 names and repository paths.
#![allow(unused_variables)] // P00 stub: remove when implemented.

use scion_core::Result;

/// A single path component (SPEC 4.4): 1-255 bytes, no NUL or `/`, not `.` or `..`, not
/// `.scion` under ASCII case-insensitive comparison.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(Vec<u8>);

impl Name {
    /// Validate a name (INVALID_ARGUMENT/invalid_path).
    pub fn new(b: &[u8]) -> Result<Self> {
        todo!("P01")
    }

    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A repository path (SPEC 4.4): names joined by `/`, at most 4096 bytes, no leading or
/// trailing `/`; the root is the empty path. Ordering is bytewise.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoPath(Vec<u8>);

impl RepoPath {
    /// Validate a path (INVALID_ARGUMENT/invalid_path). The empty input is the root.
    pub fn parse(b: &[u8]) -> Result<Self> {
        todo!("P01")
    }

    /// The root (empty) path.
    pub fn root() -> Self {
        RepoPath(Vec::new())
    }

    /// True for the root path.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The parent directory; `None` for the root.
    pub fn parent(&self) -> Option<RepoPath> {
        todo!("P01")
    }

    /// The last component; `None` for the root.
    pub fn file_name(&self) -> Option<&[u8]> {
        todo!("P01")
    }

    /// The components in order (none for the root).
    pub fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.0.split(|b| *b == b'/').filter(|c| !c.is_empty())
    }

    /// All strict ancestors, root first, excluding `self`.
    pub fn ancestors(&self) -> Vec<RepoPath> {
        todo!("P01")
    }

    /// Strict, component-wise ancestry (`a` is an ancestor of `a/b`, not of `ab`).
    pub fn is_ancestor_of(&self, other: &RepoPath) -> bool {
        todo!("P01")
    }

    /// `self/n` (INVALID_ARGUMENT/invalid_path when too long).
    pub fn join(&self, n: &Name) -> Result<RepoPath> {
        todo!("P01")
    }
}
