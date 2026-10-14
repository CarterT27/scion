//! The model's own canonical encoder (SPEC 4.1-4.3), independent of scion-object.
//!
//! Trees are built recursively from the flat [`MTree`]: the tree of a directory lists its direct
//! files and, for every subdirectory, a mode-4 entry whose id is the recursively computed tree id.

use crate::{MChangeset, MDelta, MMeta, MTree};
use std::collections::BTreeMap;

/// Algorithm byte of BLAKE3-256 ids (SPEC 4.2).
pub const ALGO_BLAKE3: u8 = 0x01;
/// Kind byte: Blob (SPEC 4.2).
pub const KIND_BLOB: u8 = 0x01;
/// Kind byte: Tree (SPEC 4.2).
pub const KIND_TREE: u8 = 0x02;
/// Kind byte: Changeset (SPEC 4.2).
pub const KIND_CHANGESET: u8 = 0x03;
/// Kind byte: Meta (SPEC 4.2).
pub const KIND_META: u8 = 0x04;

/// Append an unsigned LEB128 varint, minimally encoded (SPEC 4.1 `uvarint`).
pub fn put_uvarint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Append a zigzag-mapped signed varint (SPEC 4.1 `svarint`).
pub fn put_svarint(out: &mut Vec<u8>, v: i64) {
    put_uvarint(out, ((v << 1) ^ (v >> 63)) as u64);
}

/// Append `bytes`: uvarint length then the octets (SPEC 4.1).
pub fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_uvarint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn put_timestamp(out: &mut Vec<u8>, t: (i64, i64)) {
    put_svarint(out, t.0);
    put_svarint(out, t.1);
}

fn put_extensions(out: &mut Vec<u8>, ext: &[(u64, Vec<u8>)]) {
    put_uvarint(out, ext.len() as u64);
    for (tag, value) in ext {
        put_uvarint(out, *tag);
        put_bytes(out, value);
    }
}

/// `algo || kind || BLAKE3(bytes)` (SPEC 4.2).
pub fn make_id(kind: u8, bytes: &[u8]) -> [u8; 34] {
    let mut id = [0u8; 34];
    id[0] = ALGO_BLAKE3;
    id[1] = kind;
    id[2..].copy_from_slice(blake3::hash(bytes).as_bytes());
    id
}

/// Blob id: `0x01 0x01 || BLAKE3(content)` (SPEC 4.2, 4.3.1).
pub fn blob_id(c: &[u8]) -> [u8; 34] {
    make_id(KIND_BLOB, c)
}

/// Root tree id of a flat tree (trees built recursively; SPEC 4.3.2).
pub fn tree_id(t: &MTree) -> [u8; 34] {
    tree_id_for_dir(t, b"")
}

/// Tree id of directory `dir` (empty = root) of `t` (SPEC 4.3.2).
pub fn tree_id_for_dir(t: &MTree, dir: &[u8]) -> [u8; 34] {
    make_id(KIND_TREE, &tree_bytes_for_dir(t, dir))
}

/// Path of child `name` of directory `dir`.
fn child_path(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        name.to_vec()
    } else {
        let mut p = dir.to_vec();
        p.push(b'/');
        p.extend_from_slice(name);
        p
    }
}

/// Canonical bytes of the tree for directory `dir` (empty = root) of `t` (SPEC 4.3.2).
///
/// A directory with no files under it encodes as the empty tree.
pub fn tree_bytes_for_dir(t: &MTree, dir: &[u8]) -> Vec<u8> {
    // name -> (mode, id, size); BTreeMap gives the bytewise ascending order.
    let mut entries: BTreeMap<Vec<u8>, (u8, [u8; 34], u64)> = BTreeMap::new();
    for (path, file) in t {
        let rest: &[u8] = if dir.is_empty() {
            path
        } else if path.len() > dir.len() + 1 && path.starts_with(dir) && path[dir.len()] == b'/' {
            &path[dir.len() + 1..]
        } else {
            continue;
        };
        match rest.iter().position(|&c| c == b'/') {
            None => {
                entries.insert(
                    rest.to_vec(),
                    (file.mode, blob_id(&file.content), file.content.len() as u64),
                );
            }
            Some(i) => {
                let name = &rest[..i];
                if !entries.contains_key(name) {
                    let sub = tree_id_for_dir(t, &child_path(dir, name));
                    entries.insert(name.to_vec(), (4, sub, 0));
                }
            }
        }
    }
    let mut out = Vec::new();
    put_uvarint(&mut out, 1); // format_version
    put_uvarint(&mut out, entries.len() as u64);
    for (name, (mode, id, size)) in &entries {
        put_bytes(&mut out, name);
        out.push(*mode);
        out.extend_from_slice(id);
        put_uvarint(&mut out, *size);
    }
    put_extensions(&mut out, &[]);
    out
}

/// Canonical Meta bytes (SPEC 4.3.3).
pub fn meta_bytes(m: &MMeta) -> Vec<u8> {
    let mut out = Vec::new();
    put_uvarint(&mut out, 1); // format_version
    put_bytes(&mut out, &m.author);
    put_timestamp(&mut out, m.author_time);
    match &m.committer {
        None => out.push(0),
        Some(c) => {
            out.push(1);
            put_bytes(&mut out, c);
        }
    }
    match m.committer_time {
        None => out.push(0),
        Some(ts) => {
            out.push(1);
            put_timestamp(&mut out, ts);
        }
    }
    put_bytes(&mut out, &m.creator);
    put_bytes(&mut out, &m.message);
    put_extensions(&mut out, &m.ext);
    out
}

/// Meta id (SPEC 4.2, 4.3.3).
pub fn meta_id(m: &MMeta) -> [u8; 34] {
    make_id(KIND_META, &meta_bytes(m))
}

/// Canonical Changeset bytes (SPEC 4.3.4). Extensions are empty.
pub fn changeset_bytes(c: &MChangeset) -> Vec<u8> {
    let mut out = Vec::new();
    put_uvarint(&mut out, 1); // format_version
    put_uvarint(&mut out, c.parents.len() as u64);
    for p in &c.parents {
        out.extend_from_slice(p);
    }
    out.extend_from_slice(&c.root_tree);
    out.extend_from_slice(&c.meta);
    out.extend_from_slice(&c.change_id);
    put_uvarint(&mut out, c.delta.len() as u64);
    for (path, op) in &c.delta {
        put_bytes(&mut out, path);
        match op {
            Some((mode, id, size)) => {
                out.push(1);
                out.push(*mode);
                out.extend_from_slice(id);
                put_uvarint(&mut out, *size);
            }
            None => out.push(2),
        }
    }
    put_extensions(&mut out, &[]);
    out
}

/// Changeset id (SPEC 4.2, 4.3.4).
pub fn changeset_id(c: &MChangeset) -> [u8; 34] {
    make_id(KIND_CHANGESET, &changeset_bytes(c))
}

/// Build the structural changeset whose first-parent tree is `t1` and whose result is `tree`:
/// `root_tree = tree_id(tree)` and `delta = FileDiff(t1, tree)` with blob ids (SPEC 4.3.4).
pub fn changeset_for(
    parents: Vec<[u8; 34]>,
    t1: &MTree,
    tree: &MTree,
    meta: [u8; 34],
    change_id: [u8; 16],
) -> MChangeset {
    let delta = crate::file_diff(t1, tree)
        .into_iter()
        .map(|d| match d {
            MDelta::Put(p, f) => {
                let id = blob_id(&f.content);
                (p, Some((f.mode, id, f.content.len() as u64)))
            }
            MDelta::Delete(p) => (p, None),
        })
        .collect();
    MChangeset {
        parents,
        root_tree: tree_id(tree),
        meta,
        change_id,
        delta,
    }
}

/// SPEC 4.2 text form of a raw id: kind letter, algo digit, 64 lowercase hex digits.
///
/// Derived kind `0x10` prints as `x`; other kinds outside 1..=4 print as `x` followed by the
/// two-hex-digit kind (PLAN 6.6).
pub fn id_text(id: &[u8; 34]) -> String {
    let mut s = String::with_capacity(70);
    match id[1] {
        KIND_BLOB => s.push('b'),
        KIND_TREE => s.push('t'),
        KIND_CHANGESET => s.push('c'),
        KIND_META => s.push('m'),
        0x10 => s.push('x'),
        k => {
            s.push('x');
            s.push_str(&id[0].to_string());
            s.push_str(&format!("{k:02x}"));
            for b in &id[2..] {
                s.push_str(&format!("{b:02x}"));
            }
            return s;
        }
    }
    s.push_str(&id[0].to_string());
    for b in &id[2..] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MFile;

    #[test]
    fn varints() {
        let mut v = Vec::new();
        put_uvarint(&mut v, 0);
        put_uvarint(&mut v, 127);
        put_uvarint(&mut v, 128);
        put_uvarint(&mut v, 300);
        assert_eq!(v, vec![0, 0x7f, 0x80, 0x01, 0xac, 0x02]);
        let mut s = Vec::new();
        put_svarint(&mut s, 0);
        put_svarint(&mut s, -1);
        put_svarint(&mut s, 1);
        put_svarint(&mut s, -2);
        put_svarint(&mut s, i64::MIN);
        assert_eq!(&s[..4], &[0, 1, 2, 3]);
        let mut u = Vec::new();
        put_uvarint(&mut u, u64::MAX);
        assert_eq!(&s[4..], &u[..]);
        assert_eq!(u.len(), 10);
    }

    #[test]
    fn empty_tree_bytes() {
        assert_eq!(tree_bytes_for_dir(&MTree::new(), b""), vec![1, 0, 0]);
    }

    #[test]
    fn nested_tree_matches_manual_encoding() {
        let mut t = MTree::new();
        t.insert(
            b"d/f".to_vec(),
            MFile {
                mode: 2,
                content: b"x".to_vec(),
            },
        );
        t.insert(
            b"a".to_vec(),
            MFile {
                mode: 1,
                content: b"".to_vec(),
            },
        );
        let mut sub = vec![1, 1, 1, b'f', 2];
        sub.extend_from_slice(&blob_id(b"x"));
        sub.extend_from_slice(&[1, 0]);
        assert_eq!(tree_bytes_for_dir(&t, b"d"), sub);
        let mut root = vec![1, 2, 1, b'a', 1];
        root.extend_from_slice(&blob_id(b""));
        root.extend_from_slice(&[0, 1, b'd', 4]);
        root.extend_from_slice(&make_id(KIND_TREE, &sub));
        root.extend_from_slice(&[0, 0]);
        assert_eq!(tree_bytes_for_dir(&t, b""), root);
        assert_eq!(tree_id(&t), make_id(KIND_TREE, &root));
    }

    #[test]
    fn id_text_forms() {
        let b = blob_id(b"");
        let s = id_text(&b);
        assert!(s.starts_with("b1"));
        assert_eq!(s.len(), 66);
        assert_eq!(&s[2..], blake3::hash(b"").to_hex().as_str());
        let mut x = b;
        x[1] = 0x10;
        assert!(id_text(&x).starts_with("x1"));
        assert_eq!(id_text(&x).len(), 66);
        x[1] = 0x11;
        assert!(id_text(&x).starts_with("x111"));
        assert_eq!(id_text(&x).len(), 68);
        x[1] = KIND_CHANGESET;
        assert!(id_text(&x).starts_with("c1"));
        x[1] = KIND_META;
        assert!(id_text(&x).starts_with("m1"));
        x[1] = KIND_TREE;
        assert!(id_text(&x).starts_with("t1"));
    }
}
