//! SPEC 18.2 apply_delta (tree construction; P12).
//!
//! Only the ancestors of delta paths are visited: their `T1` trees are loaded top-down (one
//! tree read per touched directory that exists in `T1`), then the directories are rebuilt
//! deepest first (ties ascending by path). In each directory the deletes are applied first,
//! then the already rebuilt child directories, then the puts. An empty non-root directory is
//! pruned by its parent; the root may become empty. The first failure in that walk order is
//! returned, which is also the order of the independent reference model.

use crate::history::History;
use crate::types::Applied;
use bytes::Bytes;
use scion_core::{Kind, Mode, ObjectId, Result, ScionError, subcode};
use scion_object::{Canonical, DeltaEntry, DeltaOp, Extensions, RepoPath, Tree, TreeEntry};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Entries of a directory being rebuilt, keyed by name bytes (bytewise order, SPEC 4.3.2).
type Entries = BTreeMap<Vec<u8>, TreeEntry>;

fn fail(code: &str, i: usize, what: &str) -> ScionError {
    // Messages name the delta entry index, never the raw path (implementation-defined 6.9).
    ScionError::invalid(code, format!("delta entry {i}: {what}"))
}

/// SPEC 8.1 step 2 shape rules that `apply_delta` relies on: strictly ascending paths
/// (delta_unsorted / delta_duplicate), then per entry a non-root path and a valid Put
/// (invalid_path / invalid_mode / invalid_kind).
pub(crate) fn check_delta_order(delta: &[DeltaEntry]) -> Result<()> {
    for (i, pair) in delta.windows(2).enumerate() {
        if let [a, b] = pair {
            match a.path.cmp(&b.path) {
                Ordering::Less => {}
                Ordering::Equal => {
                    return Err(fail(
                        subcode::DELTA_DUPLICATE,
                        i + 1,
                        "repeats the previous path",
                    ));
                }
                Ordering::Greater => {
                    return Err(fail(
                        subcode::DELTA_UNSORTED,
                        i + 1,
                        "sorts before the previous path",
                    ));
                }
            }
        }
    }
    for e in delta {
        e.validate()?;
    }
    Ok(())
}

/// Encode a rebuilt directory (fresh tree, no extensions; SPEC 18.2 `Tree(sorted(entries))`).
fn build_tree(entries: Entries) -> (ObjectId, Bytes) {
    let tree = Tree {
        entries: entries.into_values().collect(),
        extensions: Extensions::new(),
    };
    let bytes = tree.encode();
    let id = ObjectId::hash_encoded(Kind::Tree, &bytes);
    (id, Bytes::from(bytes))
}

impl History {
    /// SPEC 18.2: build the result tree of applying `delta` to tree `t1`, walking only the
    /// ancestors of delta paths (O(|delta| x depth) tree reads).
    ///
    /// Failures (INVALID_ARGUMENT): the delta shape (delta_unsorted, delta_duplicate,
    /// invalid_path, invalid_mode, invalid_kind), then in walk order delete_absent (the path
    /// is not in `t1`), delete_not_file (it is a directory), file_directory (a rebuilt
    /// directory whose name is a file of `t1` that is not deleted, or a Put at a directory
    /// that is not fully deleted) and noop_put (the Put equals the `t1` entry).
    ///
    /// Returns the new root id plus every rebuilt non-empty tree with its directory path and
    /// canonical bytes, ascending by path. Nothing is stored.
    pub async fn apply_delta(&self, t1: &ObjectId, delta: &[DeltaEntry]) -> Result<Applied> {
        check_delta_order(delta)?;
        if delta.is_empty() {
            return Ok(Applied {
                root: *t1,
                new_trees: Vec::new(),
            });
        }

        // Touched directories and the delta entries of each directory.
        let mut touched: BTreeSet<RepoPath> = BTreeSet::new();
        let mut by_dir: BTreeMap<RepoPath, Vec<usize>> = BTreeMap::new();
        for (i, e) in delta.iter().enumerate() {
            let Some(parent) = e.path.parent() else {
                return Err(fail(subcode::INVALID_PATH, i, "path is the root"));
            };
            touched.extend(e.path.ancestors());
            by_dir.entry(parent).or_default().push(i);
        }

        // Load the T1 trees of touched directories top-down. Ascending path order visits a
        // parent (a strict prefix) before its children.
        let mut t1_trees: BTreeMap<&RepoPath, Arc<Tree>> = BTreeMap::new();
        let mut children: BTreeMap<RepoPath, Vec<&RepoPath>> = BTreeMap::new();
        for dir in &touched {
            let id = match dir.parent() {
                None => Some(*t1),
                Some(parent) => {
                    let found = match (t1_trees.get(&parent), dir.file_name()) {
                        (Some(pt), Some(name)) => {
                            pt.get(name).filter(|e| e.mode == Mode::Tree).map(|e| e.id)
                        }
                        _ => None,
                    };
                    children.entry(parent).or_default().push(dir);
                    found
                }
            };
            if let Some(id) = id {
                t1_trees.insert(dir, self.tree(&id).await?);
            }
        }

        let mut order: Vec<&RepoPath> = touched.iter().collect();
        order.sort_by(|a, b| b.depth().cmp(&a.depth()).then_with(|| a.cmp(b)));

        // dir -> rebuilt tree id (`None` = empty, pruned by its parent).
        let mut built: BTreeMap<&RepoPath, Option<ObjectId>> = BTreeMap::new();
        let mut new_trees: Vec<(RepoPath, ObjectId, Bytes)> = Vec::new();
        let mut root = *t1;
        let no_entries: Vec<usize> = Vec::new();
        for dir in order {
            let mut entries: Entries = t1_trees
                .get(dir)
                .map(|t| {
                    t.entries
                        .iter()
                        .map(|e| (e.name.as_bytes().to_vec(), e.clone()))
                        .collect()
                })
                .unwrap_or_default();
            let idxs = by_dir.get(dir).unwrap_or(&no_entries);

            // 1. Deletes.
            for &i in idxs {
                let e = &delta[i];
                if e.op != DeltaOp::Delete {
                    continue;
                }
                let name = e.path.file_name().unwrap_or_default();
                match entries.get(name) {
                    Some(t) if t.mode != Mode::Tree => {
                        entries.remove(name);
                    }
                    Some(_) => {
                        return Err(fail(subcode::DELETE_NOT_FILE, i, "delete of a directory"));
                    }
                    None => {
                        return Err(fail(subcode::DELETE_ABSENT, i, "delete of an absent path"));
                    }
                }
            }

            // 2. Rebuilt child directories (already built: deepest first).
            for child in children.get(dir).map(Vec::as_slice).unwrap_or_default() {
                let Some(name) = child.name() else {
                    continue;
                };
                if let Some(t) = entries.get(name.as_bytes())
                    && t.mode != Mode::Tree
                {
                    // The ancestor file is not deleted; report the first delta entry below it.
                    let i = delta
                        .iter()
                        .position(|e| child.is_ancestor_of(&e.path))
                        .unwrap_or_default();
                    return Err(fail(
                        subcode::FILE_DIRECTORY,
                        i,
                        "an ancestor is a file that the delta does not delete",
                    ));
                }
                match built.get(child).copied().flatten() {
                    Some(id) => {
                        entries.insert(
                            name.as_bytes().to_vec(),
                            TreeEntry {
                                name,
                                mode: Mode::Tree,
                                id,
                                size: 0,
                            },
                        );
                    }
                    None => {
                        entries.remove(name.as_bytes());
                    }
                }
            }

            // 3. Puts.
            for &i in idxs {
                let e = &delta[i];
                let DeltaOp::Put { mode, id, size } = &e.op else {
                    continue;
                };
                let Some(name) = e.path.name() else {
                    return Err(fail(subcode::INVALID_PATH, i, "path is the root"));
                };
                match entries.get(name.as_bytes()) {
                    Some(t) if t.mode == Mode::Tree => {
                        return Err(fail(
                            subcode::FILE_DIRECTORY,
                            i,
                            "put at a directory that is not fully deleted",
                        ));
                    }
                    Some(t) if t.mode == *mode && t.id == *id => {
                        return Err(fail(subcode::NOOP_PUT, i, "put equals the parent entry"));
                    }
                    _ => {}
                }
                entries.insert(
                    name.as_bytes().to_vec(),
                    TreeEntry {
                        name,
                        mode: *mode,
                        id: *id,
                        size: *size,
                    },
                );
            }

            if entries.is_empty() && !dir.is_root() {
                built.insert(dir, None);
                continue;
            }
            let nonempty = !entries.is_empty();
            let (id, bytes) = build_tree(entries);
            if nonempty {
                new_trees.push((dir.clone(), id, bytes));
            }
            if dir.is_root() {
                root = id;
            }
            built.insert(dir, Some(id));
        }
        new_trees.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Applied { root, new_trees })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HistoryConfig;
    use crate::hooks::NoEager;
    use scion_core::{ErrorCode, SystemClock, empty_tree_id};
    use scion_store::{MemOptions, ObjectRepo, ObjectRepoConfig, Storage};

    fn history() -> History {
        let (storage, _) = Storage::in_memory(MemOptions::default());
        let objects = ObjectRepo::new(storage, ObjectRepoConfig::default());
        History::new(
            objects,
            HistoryConfig::default(),
            Arc::new(NoEager),
            Arc::new(SystemClock),
        )
    }

    fn p(s: &str) -> RepoPath {
        RepoPath::parse(s.as_bytes()).unwrap()
    }

    fn put(s: &str, content: &str) -> DeltaEntry {
        DeltaEntry {
            path: p(s),
            op: DeltaOp::Put {
                mode: Mode::File,
                id: ObjectId::hash_blob(content.as_bytes()),
                size: content.len() as u64,
            },
        }
    }

    fn del(s: &str) -> DeltaEntry {
        DeltaEntry {
            path: p(s),
            op: DeltaOp::Delete,
        }
    }

    async fn store(h: &History, a: &Applied) {
        for (_, _, bytes) in &a.new_trees {
            h.objects()
                .put_encoded(Kind::Tree, bytes.clone())
                .await
                .unwrap();
        }
    }

    async fn subcode_of(h: &History, t: &ObjectId, d: Vec<DeltaEntry>) -> String {
        let e = h.apply_delta(t, &d).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument, "{e}");
        e.subcode().unwrap_or_default().to_owned()
    }

    #[tokio::test]
    async fn build_and_prune() {
        let h = history();
        let a = h
            .apply_delta(&empty_tree_id(), &[put("a/b/c", "1"), put("a/d", "2")])
            .await
            .unwrap();
        let paths: Vec<&[u8]> = a.new_trees.iter().map(|t| t.0.as_bytes()).collect();
        assert_eq!(paths, vec![b"".as_slice(), b"a", b"a/b"]);
        store(&h, &a).await;
        // Deleting the last file of a/b prunes a/b; a stays.
        let b = h.apply_delta(&a.root, &[del("a/b/c")]).await.unwrap();
        store(&h, &b).await;
        assert!(h.lookup(&b.root, &p("a/b")).await.unwrap().is_none());
        assert!(h.lookup(&b.root, &p("a/d")).await.unwrap().is_some());
        // Deleting everything leaves an empty root.
        let c = h.apply_delta(&b.root, &[del("a/d")]).await.unwrap();
        assert_eq!(c.root, empty_tree_id());
        assert!(c.new_trees.is_empty());
        // An empty delta changes nothing.
        let same = h.apply_delta(&b.root, &[]).await.unwrap();
        assert_eq!(same.root, b.root);
        assert!(same.new_trees.is_empty());
    }

    #[tokio::test]
    async fn subcodes() {
        let h = history();
        let a = h
            .apply_delta(&empty_tree_id(), &[put("d/x", "1"), put("f", "2")])
            .await
            .unwrap();
        store(&h, &a).await;
        let t = a.root;
        let cases = [
            (vec![del("nope")], subcode::DELETE_ABSENT),
            (vec![del("f/x")], subcode::DELETE_ABSENT),
            (vec![del("d")], subcode::DELETE_NOT_FILE),
            (vec![put("f", "2")], subcode::NOOP_PUT),
            (vec![put("f/x", "3")], subcode::FILE_DIRECTORY),
            (vec![put("d", "3")], subcode::FILE_DIRECTORY),
            (vec![put("b", "1"), put("a", "1")], subcode::DELTA_UNSORTED),
            (vec![put("a", "1"), del("a")], subcode::DELTA_DUPLICATE),
        ];
        for (d, want) in cases {
            assert_eq!(subcode_of(&h, &t, d).await, want);
        }
        // Transitions with the required deletes succeed.
        let ok = h
            .apply_delta(&t, &[put("d", "3"), del("d/x"), del("f"), put("f/x", "3")])
            .await
            .unwrap();
        store(&h, &ok).await;
        let d = h.lookup(&ok.root, &p("d")).await.unwrap().unwrap();
        assert_eq!(d.mode, Mode::File);
        let f = h.lookup(&ok.root, &p("f")).await.unwrap().unwrap();
        assert_eq!(f.mode, Mode::Tree);
    }
}
