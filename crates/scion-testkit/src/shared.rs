//! Shared-subtree repositories for complexity gates (SPEC 19.18; PLAN 3.5; P09).
//!
//! A repository of `files` logical files where every file sits at the same depth. The tree is
//! balanced with one fanout `f` (the smallest `f` with `f^depth >= files`) and is filled in
//! order, so every directory is either *full* (all subtrees of one level are identical) or the
//! single *partial* directory of its level. Content addressing collapses identical subtrees, so
//! the repository needs at most `2 x depth` distinct trees and `f` distinct blobs: O(depth x
//! fanout) objects and bytes, whether it holds 10^3 or 10^7 files.
//!
//! Depth convention: `depth` is the number of path components of every file, which equals the
//! number of trees on the path from the root to the file (root included). For example
//! `depth = 3` gives files like `d0/d1/f2`.

use crate::encode::{RawEntry, tree_object};
use bytes::Bytes;
use scion_core::{Kind, Mode, ObjectId};
use scion_object::RepoPath;
use scion_refmodel::{MFile, MTree};
use std::collections::{BTreeMap, BTreeSet};

/// One structural directory of the hash-consed DAG: `(subdirs, files)` where `subdirs` lists
/// `(name, node index)` and `files` lists `(name, file)`, both ascending by name.
pub type StructuralDir = (Vec<(Vec<u8>, usize)>, Vec<(Vec<u8>, MFile)>);

/// A balanced tree of identical subtrees reaching 10^3..10^7 logical files with
/// O(depth x fanout) distinct objects, perturbable along one path (SPEC 19.18).
#[derive(Clone, Debug)]
pub struct SharedSubtreeRepo {
    /// Depth: path components of every file (trees on the path, root included).
    pub depth: u32,
    /// Fanout: entries per full directory.
    pub fanout: u32,
    /// Logical files.
    pub files: u64,
    /// Hash-consed structural directories; children precede parents, the root is last.
    nodes: Vec<StructuralDir>,
}

/// Build a repository with `files` logical files at depth `depth` (SPEC 19.18).
///
/// `depth` is clamped to at least 1. The fanout is the smallest `f >= 1` with
/// `f^depth >= files`, so equal-depth repositories of 10^3, 10^5 and 10^7 files differ only in
/// fanout. `files == 0` yields the empty root.
pub fn shared_subtree_repo(files: u64, depth: u32) -> SharedSubtreeRepo {
    let depth = depth.max(1);
    let fanout = fanout_for(files, depth);
    let mut b = Builder {
        depth,
        fanout,
        width: digits(fanout.saturating_sub(1) as u64),
        nodes: Vec::new(),
        memo: BTreeMap::new(),
    };
    if files == 0 {
        b.nodes.push((Vec::new(), Vec::new()));
    } else {
        b.build(0, files);
    }
    SharedSubtreeRepo {
        depth,
        fanout,
        files,
        nodes: b.nodes,
    }
}

/// Smallest `f >= 1` with `f^depth >= files`.
fn fanout_for(files: u64, depth: u32) -> u32 {
    if files <= 1 {
        return 1;
    }
    // Start from the floating-point root and correct it exactly.
    let mut f = (files as f64).powf(1.0 / depth as f64).floor().max(1.0) as u64;
    while f > 1 && pow_sat(f - 1, depth) >= files {
        f -= 1;
    }
    while pow_sat(f, depth) < files {
        f += 1;
    }
    u32::try_from(f).unwrap_or(u32::MAX)
}

fn pow_sat(base: u64, exp: u32) -> u64 {
    let mut acc: u64 = 1;
    for _ in 0..exp {
        acc = acc.saturating_mul(base);
    }
    acc
}

fn digits(mut n: u64) -> usize {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

struct Builder {
    depth: u32,
    fanout: u32,
    width: usize,
    nodes: Vec<StructuralDir>,
    /// (level, file count) -> node index.
    memo: BTreeMap<(u32, u64), usize>,
}

impl Builder {
    fn dir_name(&self, i: u64) -> Vec<u8> {
        format!("d{:0w$}", i, w = self.width).into_bytes()
    }

    fn file_name(&self, i: u64) -> Vec<u8> {
        format!("f{:0w$}", i, w = self.width).into_bytes()
    }

    /// Build the directory at `level` (0 = root) holding the first `count` file slots.
    fn build(&mut self, level: u32, count: u64) -> usize {
        if let Some(i) = self.memo.get(&(level, count)) {
            return *i;
        }
        let node = if level + 1 >= self.depth {
            let files = (0..count)
                .map(|i| (self.file_name(i), leaf_file(i)))
                .collect();
            (Vec::new(), files)
        } else {
            let child_cap = pow_sat(self.fanout as u64, self.depth - level - 1).max(1);
            let full = count / child_cap;
            let rem = count % child_cap;
            let mut subdirs = Vec::new();
            if full > 0 {
                let child = self.build(level + 1, child_cap);
                for i in 0..full {
                    subdirs.push((self.dir_name(i), child));
                }
            }
            if rem > 0 {
                let child = self.build(level + 1, rem);
                subdirs.push((self.dir_name(full), child));
            }
            (subdirs, Vec::new())
        };
        self.nodes.push(node);
        let idx = self.nodes.len() - 1;
        self.memo.insert((level, count), idx);
        idx
    }
}

/// Content of the `i`-th file of a leaf directory: distinct per slot, identical across leaves.
fn leaf_file(i: u64) -> MFile {
    MFile {
        mode: 1,
        content: format!("shared subtree file {i}\n").into_bytes(),
    }
}

impl SharedSubtreeRepo {
    /// Only the distinct encoded objects (blobs and trees), children before parents, root tree
    /// last (SPEC 19.18: O(depth x fanout) objects). Encoded with scion-object.
    pub fn encoded_objects(&self) -> Vec<(ObjectId, Kind, Bytes)> {
        self.encode().1
    }

    /// Root tree id (SPEC 4.2), encoded with scion-object.
    pub fn root_tree(&self) -> ObjectId {
        self.encode().0
    }

    /// The path of the one perturbable file (the first file in path order), e.g. `d0/d0/f0`.
    pub fn perturb_path(&self) -> RepoPath {
        let bytes = self.perturb_path_bytes();
        match RepoPath::parse(&bytes) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "shared_subtree_repo: perturb path failed to parse");
                RepoPath::root()
            }
        }
    }

    /// Number of distinct trees (computed structurally from the hash-consed DAG; equals the
    /// number of distinct tree objects in [`SharedSubtreeRepo::encoded_objects`]).
    pub fn distinct_trees(&self) -> usize {
        self.nodes.len()
    }

    /// Raw bytes of [`SharedSubtreeRepo::perturb_path`] (no scion-object involved). Empty when
    /// the repository has no files.
    pub fn perturb_path_bytes(&self) -> Vec<u8> {
        let mut path = Vec::new();
        let mut node = self.nodes.len().saturating_sub(1);
        while let Some((subdirs, files)) = self.nodes.get(node) {
            if let Some((name, child)) = subdirs.first() {
                path.extend_from_slice(name);
                path.push(b'/');
                node = *child;
            } else {
                match files.first() {
                    Some((name, _)) => path.extend_from_slice(name),
                    None => path.clear(),
                }
                break;
            }
        }
        path
    }

    /// The hash-consed structural DAG (SPEC 19.18): directory nodes with children before
    /// parents; the root is the last node. Distinct nodes are distinct subtrees.
    pub fn structural_dirs(&self) -> &[StructuralDir] {
        &self.nodes
    }

    /// Index of the root in [`SharedSubtreeRepo::structural_dirs`].
    pub fn root_node(&self) -> usize {
        self.nodes.len().saturating_sub(1)
    }

    /// Distinct blob contents (structural): one per file slot of a full leaf directory.
    pub fn distinct_blobs(&self) -> usize {
        let mut set: BTreeSet<&[u8]> = BTreeSet::new();
        for (_, files) in &self.nodes {
            for (_, f) in files {
                set.insert(&f.content);
            }
        }
        set.len()
    }

    /// The full file-level tree. Cost is O(files): use only for small repositories (for
    /// example 10^3 files) when comparing with an oracle.
    pub fn materialize(&self) -> MTree {
        let mut out = MTree::new();
        let mut stack: Vec<(usize, Vec<u8>)> = vec![(self.root_node(), Vec::new())];
        while let Some((node, prefix)) = stack.pop() {
            let Some((subdirs, files)) = self.nodes.get(node) else {
                continue;
            };
            for (name, f) in files {
                let mut p = prefix.clone();
                p.extend_from_slice(name);
                out.insert(p, f.clone());
            }
            for (name, child) in subdirs {
                let mut p = prefix.clone();
                p.extend_from_slice(name);
                p.push(b'/');
                stack.push((*child, p));
            }
        }
        out
    }

    fn encode(&self) -> (ObjectId, Vec<(ObjectId, Kind, Bytes)>) {
        let mut ids: Vec<ObjectId> = Vec::with_capacity(self.nodes.len());
        let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
        let mut objects = Vec::new();
        let root = self.root_node();
        let mut root_id = scion_core::empty_tree_id();
        for (i, (subdirs, files)) in self.nodes.iter().enumerate() {
            let mut entries: Vec<RawEntry> = Vec::with_capacity(subdirs.len() + files.len());
            for (name, f) in files {
                let id = ObjectId::hash_blob(&f.content);
                if seen.insert(id) {
                    objects.push((id, Kind::Blob, Bytes::from(f.content.clone())));
                }
                entries.push((name.clone(), Mode::File, id, f.content.len() as u64));
            }
            for (name, child) in subdirs {
                if let Some(id) = ids.get(*child) {
                    entries.push((name.clone(), Mode::Tree, *id, 0));
                }
            }
            let id = match tree_object(entries, i == root) {
                Some((id, bytes)) => {
                    if seen.insert(id) {
                        objects.push((id, Kind::Tree, bytes));
                    }
                    id
                }
                None => scion_core::empty_tree_id(),
            };
            ids.push(id);
            if i == root {
                root_id = id;
            }
        }
        (root_id, objects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fanout_is_minimal() {
        assert_eq!(fanout_for(0, 4), 1);
        assert_eq!(fanout_for(1, 4), 1);
        assert_eq!(fanout_for(1000, 3), 10);
        assert_eq!(fanout_for(1001, 3), 11);
        assert_eq!(fanout_for(10_000_000, 7), 10);
        assert_eq!(fanout_for(100_000, 5), 10);
        assert_eq!(fanout_for(1000, 8), 3);
    }

    #[test]
    fn small_repo_materializes_exactly() {
        let r = shared_subtree_repo(10, 2);
        assert_eq!(r.fanout, 4);
        let t = r.materialize();
        assert_eq!(t.len(), 10);
        assert!(t.contains_key(b"d0/f0".as_slice()));
        assert!(t.contains_key(b"d2/f1".as_slice()));
        assert!(!t.contains_key(b"d2/f2".as_slice()));
        assert_eq!(r.perturb_path_bytes(), b"d0/f0".to_vec());
    }

    #[test]
    fn empty_repo() {
        let r = shared_subtree_repo(0, 3);
        assert_eq!(r.materialize().len(), 0);
        assert_eq!(r.distinct_trees(), 1);
        assert!(r.perturb_path_bytes().is_empty());
    }
}
