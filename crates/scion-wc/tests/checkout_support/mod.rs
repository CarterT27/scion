//! Shared helpers for the P18 `checkout_*`, `goto_*`, `sparse_*` and `fetch_*` tests.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // test helpers

use bytes::Bytes;
use scion_core::{ChangeId, Kind, Mode, ObjectId};
use scion_object::{Canonical, Changeset, Extensions, Name, RepoPath, Tree, TreeEntry};
use scion_proto::ScionClient;
use scion_testkit::{FakeUpstream, FakeUpstreamOpts};
use scion_wc::testing::Snapshot;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The principal of the test token.
pub const PRINCIPAL: &str = "github:alice";
/// The bearer token mapped to [`PRINCIPAL`].
pub const TOKEN: &str = "alice-token";

/// Structural file map: path -> (mode, content or symlink target).
pub type Files = BTreeMap<Vec<u8>, (Mode, Vec<u8>)>;

/// Parse a repository path.
pub fn rp(s: &str) -> RepoPath {
    RepoPath::parse(s.as_bytes()).unwrap()
}

/// Build a file map from `(path, mode, content)` triples (mode 1 file, 2 exec, 3 symlink).
pub fn files(v: &[(&str, u8, &str)]) -> Files {
    v.iter()
        .map(|(p, m, c)| {
            (
                p.as_bytes().to_vec(),
                (Mode::from_u8(*m).unwrap(), c.as_bytes().to_vec()),
            )
        })
        .collect()
}

/// Encode a file map with scion-object: the root tree id plus every tree and blob.
pub fn encode_files(f: &Files) -> (ObjectId, Vec<(ObjectId, Bytes)>) {
    let mut objs = Vec::new();
    let root = encode_dir(f, b"", &mut objs);
    (root, objs)
}

fn encode_dir(f: &Files, prefix: &[u8], objs: &mut Vec<(ObjectId, Bytes)>) -> ObjectId {
    let mut entries: BTreeMap<Vec<u8>, TreeEntry> = BTreeMap::new();
    let mut subdirs: BTreeSet<Vec<u8>> = BTreeSet::new();
    for (p, (m, c)) in f {
        let rest: &[u8] = if prefix.is_empty() {
            p
        } else if p.len() > prefix.len() && p.starts_with(prefix) && p[prefix.len()] == b'/' {
            &p[prefix.len() + 1..]
        } else {
            continue;
        };
        match rest.iter().position(|b| *b == b'/') {
            Some(i) => {
                subdirs.insert(rest[..i].to_vec());
            }
            None => {
                let id = ObjectId::hash_blob(c);
                objs.push((id, Bytes::from(c.clone())));
                entries.insert(
                    rest.to_vec(),
                    TreeEntry {
                        name: Name::new(rest).unwrap(),
                        mode: *m,
                        id,
                        size: c.len() as u64,
                    },
                );
            }
        }
    }
    for d in subdirs {
        let mut sub = prefix.to_vec();
        if !sub.is_empty() {
            sub.push(b'/');
        }
        sub.extend_from_slice(&d);
        let id = encode_dir(f, &sub, objs);
        entries.insert(
            d.clone(),
            TreeEntry {
                name: Name::new(&d).unwrap(),
                mode: Mode::Tree,
                id,
                size: 0,
            },
        );
    }
    let tree = Tree {
        entries: entries.into_values().collect(),
        extensions: Extensions::new(),
    };
    let raw = tree.encode();
    let id = ObjectId::hash_encoded(Kind::Tree, &raw);
    objs.push((id, Bytes::from(raw)));
    id
}

/// The tree id of directory `dir` of `f` (the root for `""`).
pub fn dir_tree_id(f: &Files, dir: &str) -> ObjectId {
    let mut objs = Vec::new();
    encode_dir(f, dir.as_bytes(), &mut objs)
}

/// Hand-encode a tree WITHOUT validation (malicious-tree mode): `(name, mode, id, size)`
/// entries in the given order.
pub fn raw_tree(entries: &[(&[u8], Mode, ObjectId, u64)]) -> (ObjectId, Bytes) {
    let mut e = scion_object::enc::Encoder::new();
    e.uvarint(1);
    e.uvarint(entries.len() as u64);
    for (name, mode, id, size) in entries {
        e.bytes(name);
        e.u8(*mode as u8);
        e.id(id);
        e.uvarint(*size);
    }
    e.uvarint(0); // no extensions
    let raw = e.finish();
    (ObjectId::hash_encoded(Kind::Tree, &raw), Bytes::from(raw))
}

/// A FakeUpstream plus an authenticated client.
pub struct Env {
    pub up: FakeUpstream,
    pub client: ScionClient,
    next: AtomicU64,
}

impl Env {
    pub async fn start() -> Env {
        let up = FakeUpstream::start(FakeUpstreamOpts {
            principal_of: Arc::new(|t| (t == TOKEN).then(|| PRINCIPAL.to_owned())),
            ..Default::default()
        })
        .await;
        let client = ScionClient::connect(&up.endpoint(), up.tls(), Some(TOKEN.to_owned()))
            .await
            .unwrap();
        Env {
            up,
            client,
            next: AtomicU64::new(1),
        }
    }

    /// Insert every object of `f`; returns the root tree.
    pub fn seed(&self, f: &Files) -> ObjectId {
        let (root, objs) = encode_files(f);
        for (id, raw) in objs {
            self.up.insert_object(id, raw, false);
        }
        root
    }

    /// Insert a changeset with `root` as its root tree.
    pub fn changeset(&self, root: ObjectId) -> ObjectId {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let mut digest = [0u8; 32];
        digest[..8].copy_from_slice(&n.to_be_bytes());
        let mut cid = [0u8; 16];
        cid[..8].copy_from_slice(&n.to_be_bytes());
        let cs = Changeset {
            parents: Vec::new(),
            root_tree: root,
            meta: ObjectId::new(Kind::Meta, digest),
            change_id: ChangeId(cid),
            delta: Vec::new(),
            extensions: Extensions::new(),
        };
        let id = cs.id();
        self.up.insert_changeset(id, Bytes::from(cs.encode()));
        id
    }

    /// Seed `f`, create a changeset and point ref `name` at it (new version each call).
    pub fn commit(&self, f: &Files, name: &str) -> ObjectId {
        let root = self.seed(f);
        let cs = self.changeset(root);
        let v = self.next.fetch_add(1, Ordering::Relaxed);
        self.up.set_ref(name, cs, v);
        cs
    }

    /// A scenario over this upstream's client.
    pub fn scenario(&self) -> scion_wc::testing::Scenario {
        scion_wc::testing::Scenario::new(self.client.clone()).unwrap()
    }
}

/// Convert a snapshot into a file map.
pub fn snapshot_files(s: &Snapshot) -> Files {
    s.iter()
        .map(|(p, (m, c))| (p.as_bytes().to_vec(), (*m, c.clone())))
        .collect()
}

/// The files a working copy with profile `sparse` materializes (SPEC 11.2): a profile entry
/// equals the file or is an ancestor directory; `""` covers everything. Mirrors
/// `scion_refmodel::materialize`, which scion-wc cannot dev-depend on yet.
pub fn materialize(f: &Files, sparse: &[Vec<u8>]) -> Files {
    f.iter()
        .filter(|(p, _)| {
            sparse.iter().any(|s| {
                s.is_empty()
                    || *p == s
                    || (p.len() > s.len() && p.starts_with(s) && p[s.len()] == b'/')
            })
        })
        .map(|(p, v)| (p.clone(), v.clone()))
        .collect()
}

/// Build a file map from `(path, mode, content)` triples, e.g. a generated corpus tree
/// mapped with `t.iter().map(|(p, f)| (p.clone(), f.mode, f.content.clone()))`.
pub fn from_triples(it: impl IntoIterator<Item = (Vec<u8>, u8, Vec<u8>)>) -> Files {
    it.into_iter()
        .map(|(p, m, c)| (p, (Mode::from_u8(m).unwrap(), c)))
        .collect()
}

/// Fresh empty directory.
pub fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// Names of the entries directly in `dir`.
pub fn ls(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}
