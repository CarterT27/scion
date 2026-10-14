//! Tree reading through the ObjectRepo caches (SPEC 4.3.2, 4.3.4; P12).

use crate::history::History;
use crate::types::ChangesetRecord;
use scion_core::{ObjectId, Result, empty_tree_id};
use scion_object::{RepoPath, Tree, TreeEntry};
use std::sync::Arc;

impl History {
    /// A decoded tree (SPEC 4.3.2), read through the ObjectRepo decoded-tree LRU. The empty
    /// tree is a constant and is returned without IO.
    pub async fn tree(&self, id: &ObjectId) -> Result<Arc<Tree>> {
        if *id == empty_tree_id() {
            return Ok(Arc::new(Tree::empty()));
        }
        self.objects.get_tree(id).await
    }

    /// The entry at `path` under `root` (SPEC 4.3.2, 4.4). `None` when absent, when an
    /// intermediate component is a file, and for the root path (which has no entry).
    pub async fn lookup(&self, root: &ObjectId, path: &RepoPath) -> Result<Option<TreeEntry>> {
        if path.is_root() {
            return Ok(None);
        }
        let mut current = *root;
        let mut comps = path.components().peekable();
        while let Some(name) = comps.next() {
            let tree = self.tree(&current).await?;
            let Some(entry) = tree.get(name) else {
                return Ok(None);
            };
            if comps.peek().is_none() {
                return Ok(Some(entry.clone()));
            }
            if entry.mode != scion_core::Mode::Tree {
                return Ok(None);
            }
            current = entry.id;
        }
        Ok(None)
    }

    /// A decoded changeset with its commit-graph columns (SPEC 4.3.4, 9.2), read through the
    /// ObjectRepo changeset LRU (stale read first, strong retry on a miss, SPEC 5.4).
    pub async fn changeset(&self, id: &ObjectId) -> Result<ChangesetRecord> {
        let (changeset, row) = self.objects.get_changeset(id).await?;
        Ok(ChangesetRecord {
            id: *id,
            changeset,
            generation: row.generation,
            skip: row.skip,
            created_by: row.created_by,
        })
    }
}
