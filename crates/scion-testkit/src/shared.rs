//! Shared-subtree repositories for complexity gates (SPEC 19.18; PLAN 3.5; P09).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use bytes::Bytes;
use scion_core::{Kind, ObjectId};
use scion_object::RepoPath;

/// A balanced tree of identical subtrees reaching 10^3..10^7 logical files with
/// O(depth x fanout) distinct objects, perturbable along one path.
#[derive(Clone, Debug)]
pub struct SharedSubtreeRepo {
    /// Depth.
    pub depth: u32,
    /// Fanout.
    pub fanout: u32,
    /// Logical files.
    pub files: u64,
    objects: Vec<(ObjectId, Kind, Bytes)>,
}

/// Build a repository with `files` logical files at depth `depth`.
pub fn shared_subtree_repo(files: u64, depth: u32) -> SharedSubtreeRepo {
    todo!("P09")
}

impl SharedSubtreeRepo {
    /// Only the distinct encoded objects (blobs and trees).
    pub fn encoded_objects(&self) -> Vec<(ObjectId, Kind, Bytes)> {
        todo!("P09")
    }

    /// Root tree id.
    pub fn root_tree(&self) -> ObjectId {
        todo!("P09")
    }

    /// The path of the one perturbable file.
    pub fn perturb_path(&self) -> RepoPath {
        todo!("P09")
    }

    /// Number of distinct trees.
    pub fn distinct_trees(&self) -> usize {
        todo!("P09")
    }
}
