//! Seeded corpus generator (SPEC 19.0 Corpus; P09).
#![allow(unused_variables)] // P00 stub: remove when implemented.

/// Shape of a generated repository.
#[derive(Clone, Debug)]
pub struct CorpusSpec {
    /// Files in the final tree (approximate).
    pub files: u64,
    /// Maximum directory depth.
    pub depth: u32,
    /// Entries per directory.
    pub fanout: u32,
    /// Number of commits.
    pub history_len: u32,
    /// Fraction of two-parent merges.
    pub merge_ratio: f64,
    /// Fraction of octopus merges.
    pub octopus_ratio: f64,
    /// Fraction of binary files.
    pub binary_ratio: f64,
    /// Fraction of symlinks.
    pub symlink_ratio: f64,
    /// RNG seed (output is deterministic per seed).
    pub seed: u64,
}

/// One generated commit: parents by index plus a full structural snapshot.
#[derive(Clone, Debug)]
pub struct GenCommit {
    /// Parent indices (earlier commits).
    pub parents: Vec<usize>,
    /// Full file-level snapshot.
    pub tree: scion_refmodel::MTree,
    /// Author bytes.
    pub author: Vec<u8>,
    /// Message bytes.
    pub message: Vec<u8>,
    /// Change id.
    pub change_id: [u8; 16],
}

/// A generated history in topological order.
#[derive(Clone, Debug)]
pub struct GenRepo {
    /// Commits, parents first.
    pub commits: Vec<GenCommit>,
}

/// Generate a repository from `spec` (rand_chacha, deterministic).
pub fn generate(spec: &CorpusSpec) -> GenRepo {
    todo!("P09")
}
