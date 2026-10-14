//! FileDiff between stored trees with identical-subtree pruning (SPEC 4.3.4; P12).
//!
//! Both trees are walked together; a pair of directories with equal ids is skipped without
//! reading it, so the cost is proportional to the entries of the directories that differ.
//! Entries are merge-joined (tree entries are sorted by name, SPEC 4.3.2).

use crate::history::History;
use crate::types::FileChange;
use scion_core::{EntryInfo, Mode, ObjectId, Result};
use scion_object::{RepoPath, Tree, TreeEntry};
use std::cmp::Ordering;
use std::sync::Arc;

/// `true` when file `p` lies in one of the scope prefixes (`None` = everything).
fn file_in_scope(scope: Option<&[RepoPath]>, p: &RepoPath) -> bool {
    scope.is_none_or(|s| s.iter().any(|s| s == p || s.is_ancestor_of(p)))
}

/// `true` when directory `d` contains, equals or lies below a scope prefix.
fn dir_in_scope(scope: Option<&[RepoPath]>, d: &RepoPath) -> bool {
    scope.is_none_or(|s| {
        s.iter()
            .any(|s| s == d || s.is_ancestor_of(d) || d.is_ancestor_of(s))
    })
}

fn info(e: &TreeEntry) -> EntryInfo {
    EntryInfo {
        mode: e.mode,
        id: e.id,
        size: e.size,
    }
}

impl History {
    /// SPEC 4.3.4 FileDiff from tree `a` (`None` = the empty tree) to tree `b`: every
    /// file-level path whose `(mode, id)` differs, with the old and new entries, ascending by
    /// path. Identical subtrees are pruned without being read. With `scope`, only files equal
    /// to or below one of the given paths are reported (the root path selects everything) and
    /// directories outside every scope are not read.
    pub async fn file_diff(
        &self,
        a: Option<&ObjectId>,
        b: &ObjectId,
        scope: Option<&[RepoPath]>,
    ) -> Result<Vec<FileChange>> {
        let mut out: Vec<FileChange> = Vec::new();
        let mut stack: Vec<(RepoPath, Option<ObjectId>, Option<ObjectId>)> =
            vec![(RepoPath::root(), a.copied(), Some(*b))];
        while let Some((prefix, ta, tb)) = stack.pop() {
            if ta == tb || !dir_in_scope(scope, &prefix) {
                continue;
            }
            let load = |id: Option<ObjectId>| async move {
                match id {
                    Some(id) => self.tree(&id).await.map(Some),
                    None => Ok::<Option<Arc<Tree>>, scion_core::ScionError>(None),
                }
            };
            let (sa, sb) = (load(ta).await?, load(tb).await?);
            let empty: &[TreeEntry] = &[];
            let ea = sa.as_deref().map_or(empty, |t| t.entries.as_slice());
            let eb = sb.as_deref().map_or(empty, |t| t.entries.as_slice());
            let (mut i, mut j) = (0usize, 0usize);
            while i < ea.len() || j < eb.len() {
                let (fa, fb) = match (ea.get(i), eb.get(j)) {
                    (Some(x), Some(y)) => match x.name.as_bytes().cmp(y.name.as_bytes()) {
                        Ordering::Less => (Some(x), None),
                        Ordering::Greater => (None, Some(y)),
                        Ordering::Equal => (Some(x), Some(y)),
                    },
                    (x, y) => (x, y),
                };
                let Some(name) = fa.or(fb).map(|e| &e.name) else {
                    break;
                };
                if fa.is_some() {
                    i += 1;
                }
                if fb.is_some() {
                    j += 1;
                }
                let path = prefix.join(name)?;
                let file = |e: Option<&TreeEntry>| e.filter(|e| e.mode != Mode::Tree).map(info);
                let dir = |e: Option<&TreeEntry>| e.filter(|e| e.mode == Mode::Tree).map(|e| e.id);
                let (old, new) = (file(fa), file(fb));
                let same = match (&old, &new) {
                    (Some(x), Some(y)) => x.mode == y.mode && x.id == y.id,
                    (None, None) => true,
                    _ => false,
                };
                if !same && file_in_scope(scope, &path) {
                    out.push(FileChange {
                        path: path.clone(),
                        old,
                        new,
                    });
                }
                let (da, db) = (dir(fa), dir(fb));
                if da != db {
                    stack.push((path, da, db));
                }
            }
        }
        out.sort_by(|x, y| x.path.cmp(&y.path));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HistoryConfig;
    use crate::hooks::NoEager;
    use scion_core::{Kind, SystemClock, empty_tree_id};
    use scion_object::{DeltaEntry, DeltaOp};
    use scion_store::{MemOptions, ObjectRepo, ObjectRepoConfig, Storage};

    fn p(s: &str) -> RepoPath {
        RepoPath::parse(s.as_bytes()).unwrap()
    }

    fn put(s: &str, c: &str) -> DeltaEntry {
        DeltaEntry {
            path: p(s),
            op: DeltaOp::Put {
                mode: Mode::File,
                id: ObjectId::hash_blob(c.as_bytes()),
                size: c.len() as u64,
            },
        }
    }

    #[tokio::test]
    async fn diff_scope_and_pruning() {
        let (storage, _) = Storage::in_memory(MemOptions::default());
        let h = History::new(
            ObjectRepo::new(storage, ObjectRepoConfig::default()),
            HistoryConfig::default(),
            Arc::new(NoEager),
            Arc::new(SystemClock),
        );
        let d1 = vec![put("a.txt", "1"), put("a/x", "1"), put("b/y", "1")];
        let a1 = h.apply_delta(&empty_tree_id(), &d1).await.unwrap();
        for (_, _, b) in &a1.new_trees {
            h.objects()
                .put_encoded(Kind::Tree, b.clone())
                .await
                .unwrap();
        }
        let d2 = vec![put("a/x", "2"), put("c", "3")];
        let a2 = h.apply_delta(&a1.root, &d2).await.unwrap();
        for (_, _, b) in &a2.new_trees {
            h.objects()
                .put_encoded(Kind::Tree, b.clone())
                .await
                .unwrap();
        }
        let full = h.file_diff(Some(&a1.root), &a2.root, None).await.unwrap();
        let paths: Vec<&[u8]> = full.iter().map(|c| c.path.as_bytes()).collect();
        assert_eq!(paths, vec![b"a/x".as_slice(), b"c"]);
        assert!(full[0].old.is_some() && full[0].new.is_some());
        assert!(full[1].old.is_none());

        let from_empty = h.file_diff(None, &a1.root, None).await.unwrap();
        let paths: Vec<&[u8]> = from_empty.iter().map(|c| c.path.as_bytes()).collect();
        // Ascending bytewise: "a.txt" < "a/x" because '.' < '/'.
        assert_eq!(paths, vec![b"a.txt".as_slice(), b"a/x", b"b/y"]);

        let scope = [p("a")];
        let scoped = h
            .file_diff(Some(&a1.root), &a2.root, Some(&scope))
            .await
            .unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].path, p("a/x"));
        let none = h
            .file_diff(Some(&a1.root), &a2.root, Some(&[]))
            .await
            .unwrap();
        assert!(none.is_empty());
        let root_scope = [RepoPath::root()];
        let all = h
            .file_diff(Some(&a1.root), &a2.root, Some(&root_scope))
            .await
            .unwrap();
        assert_eq!(all, full);

        // Pruning: an identical subtree "b" is never read.
        let before = h.objects().storage().counters.snapshot();
        h.file_diff(Some(&a1.root), &a2.root, None).await.unwrap();
        let reads = (h.objects().storage().counters.snapshot() - before).trees_read;
        // root (both sides) + "a" (both sides).
        assert_eq!(reads, 4);
    }
}
