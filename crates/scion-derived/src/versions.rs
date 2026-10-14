//! All versions of a path (SPEC 13.2 `--all-versions` redaction tooling; P24).
//!
//! The history nodes of a path (SPEC 9.2) skip what TREESAME simplification hides: a version
//! that only lived on a side branch discarded by a merge, or a version before the path was
//! deleted and re-created, is not reachable through the path's node parents. Redaction tooling
//! must find every distinct blob the path ever held (SPEC 13.2, 19.13), so [`all_versions`]
//! walks every changeset reachable from `at` and reads the path in each tree. Lookups are
//! memoized per `(depth, tree id)`, so an unchanged subtree costs one map hit per changeset.

use crate::engine::DerivedEngine;
use crate::types::DeriveCtx;
use scion_core::{Mode, ObjectId, Result};
use scion_object::RepoPath;
use std::collections::{HashMap, HashSet, VecDeque};

/// Resolves one fixed path in many root trees, memoizing per `(depth, tree id)`.
struct PathLookup<'a> {
    cx: &'a dyn DeriveCtx,
    components: Vec<Vec<u8>>,
    /// `(depth, tree at that depth) -> entry at the end of the path below it`.
    memo: HashMap<(usize, ObjectId), Option<(Mode, ObjectId)>>,
}

impl<'a> PathLookup<'a> {
    fn new(cx: &'a dyn DeriveCtx, path: &RepoPath) -> Self {
        PathLookup {
            cx,
            components: path.components().map(<[u8]>::to_vec).collect(),
            memo: HashMap::new(),
        }
    }

    /// The `(mode, id)` at the path below `root`, if any.
    async fn entry(&mut self, root: ObjectId) -> Result<Option<(Mode, ObjectId)>> {
        if self.components.is_empty() {
            return Ok(Some((Mode::Tree, root)));
        }
        // Descend until a memoized level or the end, then fill the memo back up.
        let mut visited: Vec<(usize, ObjectId)> = Vec::new();
        let mut tree = root;
        let mut depth = 0usize;
        let result = loop {
            if let Some(r) = self.memo.get(&(depth, tree)) {
                break *r;
            }
            visited.push((depth, tree));
            let t = self.cx.tree(&tree).await?;
            let found = self
                .components
                .get(depth)
                .and_then(|name| t.get(name))
                .map(|e| (e.mode, e.id));
            match found {
                Some((m, id)) if depth + 1 == self.components.len() => break Some((m, id)),
                Some((Mode::Tree, id)) => {
                    tree = id;
                    depth += 1;
                }
                _ => break None,
            }
        };
        for k in visited {
            self.memo.insert(k, result);
        }
        Ok(result)
    }
}

/// Distinct blob ids ever held at `path` in history reachable from `at`, first-seen order
/// (SPEC 13.2 `--all-versions`, 19.13).
///
/// Breadth-first over every changeset reachable from `at` (parents in order); every changeset
/// where `path` is a file (regular, executable or symlink) contributes its blob id. Versions
/// from before a deletion and re-creation of the path, and versions only on merged-away side
/// branches, are included. NOT_FOUND when `at` (or a stored ancestor) is missing.
pub async fn all_versions(
    e: &DerivedEngine,
    at: &ObjectId,
    path: &RepoPath,
) -> Result<Vec<ObjectId>> {
    let cx = e.ctx();
    let mut lookup = PathLookup::new(cx, path);
    let mut out: Vec<ObjectId> = Vec::new();
    let mut found: HashSet<ObjectId> = HashSet::new();
    let mut seen: HashSet<ObjectId> = HashSet::new();
    let mut queue: VecDeque<ObjectId> = VecDeque::from([*at]);
    while let Some(cs) = queue.pop_front() {
        if !seen.insert(cs) {
            continue;
        }
        let rec = cx.changeset(&cs).await?;
        if let Some((mode, id)) = lookup.entry(rec.changeset.root_tree).await?
            && mode != Mode::Tree
            && found.insert(id)
        {
            out.push(id);
        }
        queue.extend(rec.changeset.parents.iter().copied());
    }
    tracing::debug!(
        changesets = seen.len(),
        versions = out.len(),
        "all_versions walked"
    );
    Ok(out)
}
