//! Production encoding (scion-object) of structural trees (P09; exercised by P17).
//!
//! SPEC 4.2 (identifiers), 4.3.1 (blobs), 4.3.2 (trees), 4.4 (path rules). The structural
//! [`scion_refmodel::MTree`] is file-level; this module derives every directory tree from the
//! file paths and encodes it with [`scion_object::Tree`] and [`scion_object::Canonical`], so the
//! ids are exactly the ones production code computes.

use bytes::Bytes;
use scion_core::{Kind, Mode, ObjectId};
use scion_object::{Canonical, Extensions, Name, Tree, TreeEntry};
use std::collections::{BTreeMap, BTreeSet};

/// One entry of a directory being encoded: `(name, mode, id, size)`.
pub(crate) type RawEntry = (Vec<u8>, Mode, ObjectId, u64);

/// Encode one directory with scion-object (SPEC 4.3.2). Entries are sorted bytewise by name;
/// entries whose names violate SPEC 4.4 are skipped (and logged). Returns `None` when no valid
/// entry remains and `root` is false (a non-root tree must not be empty).
pub(crate) fn tree_object(mut entries: Vec<RawEntry>, root: bool) -> Option<(ObjectId, Bytes)> {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.dedup_by(|a, b| a.0 == b.0);
    let mut tree_entries = Vec::with_capacity(entries.len());
    for (name, mode, id, size) in entries {
        match Name::new(&name) {
            Ok(name) => tree_entries.push(TreeEntry {
                name,
                mode,
                id,
                size,
            }),
            Err(e) => {
                tracing::warn!(error = %e, "encode_tree: skipping entry with an invalid name");
            }
        }
    }
    if tree_entries.is_empty() && !root {
        return None;
    }
    let tree = Tree {
        entries: tree_entries,
        extensions: Extensions::new(),
    };
    let bytes = tree.encode();
    let id = ObjectId::hash_encoded(Kind::Tree, &bytes);
    Some((id, Bytes::from(bytes)))
}

/// A child of a directory while grouping file paths.
enum Child<'a> {
    File(&'a scion_refmodel::MFile),
    Dir,
}

/// Encode `t` with scion-object: the root tree id plus every blob and tree object.
///
/// SPEC 4.3.2 / 4.2: blobs are identified by BLAKE3 of the raw content; trees by BLAKE3 of the
/// canonical encoding. The returned object list is deduplicated by id and ordered children
/// before parents (blobs and subtrees precede the trees that reference them, matching the
/// SPEC 5.3 write order); the root tree is always last, also when `t` is empty.
///
/// Structural inputs that cannot be represented are handled deterministically: paths with an
/// empty component or a name that violates SPEC 4.4 are skipped; when a path is both a file and
/// a directory prefix of another file, the directory wins; a mode outside `1..=3` is skipped.
/// Every skip is logged with `tracing::warn!`.
pub fn encode_tree(t: &scion_refmodel::MTree) -> (ObjectId, Vec<(ObjectId, Kind, Bytes)>) {
    // dir path -> name -> child.
    let mut dirs: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, Child<'_>>> = BTreeMap::new();
    dirs.insert(Vec::new(), BTreeMap::new());
    for (path, file) in t {
        let comps: Vec<&[u8]> = path.split(|b| *b == b'/').collect();
        if comps.iter().any(|c| c.is_empty()) {
            tracing::warn!("encode_tree: skipping a path with an empty component");
            continue;
        }
        let mut dir: Vec<u8> = Vec::new();
        let last = comps.len() - 1;
        for (i, comp) in comps.iter().enumerate() {
            let entry = dirs.entry(dir.clone()).or_default();
            if i == last {
                // A directory of the same name wins over the file.
                entry.entry(comp.to_vec()).or_insert(Child::File(file));
            } else {
                // The directory replaces a file of the same name.
                entry.insert(comp.to_vec(), Child::Dir);
                if !dir.is_empty() {
                    dir.push(b'/');
                }
                dir.extend_from_slice(comp);
            }
        }
    }

    // Deepest directories first, so every subtree id is known before its parent is encoded.
    let mut order: Vec<&Vec<u8>> = dirs.keys().collect();
    order.sort_by(|a, b| {
        let da = if a.is_empty() {
            0
        } else {
            a.iter().filter(|c| **c == b'/').count() + 1
        };
        let db = if b.is_empty() {
            0
        } else {
            b.iter().filter(|c| **c == b'/').count() + 1
        };
        db.cmp(&da).then_with(|| a.cmp(b))
    });

    let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
    let mut objects: Vec<(ObjectId, Kind, Bytes)> = Vec::new();
    let mut tree_ids: BTreeMap<&[u8], ObjectId> = BTreeMap::new();
    let mut root_id = scion_core::empty_tree_id();
    for dir in order {
        let Some(children) = dirs.get(dir) else {
            continue;
        };
        let mut entries: Vec<RawEntry> = Vec::with_capacity(children.len());
        for (name, child) in children {
            match child {
                Child::File(f) => {
                    let mode = match Mode::from_u8(f.mode) {
                        Ok(m) if m.is_file_like() => m,
                        _ => {
                            tracing::warn!(mode = f.mode, "encode_tree: skipping an invalid mode");
                            continue;
                        }
                    };
                    let id = ObjectId::hash_blob(&f.content);
                    if seen.insert(id) {
                        objects.push((id, Kind::Blob, Bytes::from(f.content.clone())));
                    }
                    entries.push((name.clone(), mode, id, f.content.len() as u64));
                }
                Child::Dir => {
                    let mut sub = dir.clone();
                    if !sub.is_empty() {
                        sub.push(b'/');
                    }
                    sub.extend_from_slice(name);
                    // Absent when every entry below was skipped (no empty non-root trees).
                    if let Some(id) = tree_ids.get(sub.as_slice()) {
                        entries.push((name.clone(), Mode::Tree, *id, 0));
                    }
                }
            }
        }
        let root = dir.is_empty();
        if let Some((id, bytes)) = tree_object(entries, root) {
            // The root never equals one of its own subtrees, so it is emitted last.
            if seen.insert(id) {
                objects.push((id, Kind::Tree, bytes));
            }
            if root {
                root_id = id;
            } else {
                tree_ids.insert(dir.as_slice(), id);
            }
        }
    }
    (root_id, objects)
}
