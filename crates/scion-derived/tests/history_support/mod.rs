//! Shared helpers for the P21 history-node and git_mapping tests (SPEC 9.2, 19.6, 19.18):
//! a mock [`DeriveCtx`] (an in-memory map), structural-to-production conversion, corpus loaders
//! (through the mock and through `History::create_changeset` with a `DerivedEngine`), and node
//! walks compared with the reference model.
#![allow(dead_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use scion_core::{
    ChangeId, ErrorCode, Kind, ManualClock, Mode, ObjectId, Principal, Result, ScionError,
    empty_tree_id,
};
use scion_derived::history::{self, HistoryType};
use scion_derived::{DeriveConfig, DeriveCtx, DerivedEngine, EagerType, Registry};
use scion_history::{
    AllowAll, ChangesetRecord, CreateRequest, EagerInput, History, HistoryConfig, MetaFields,
};
use scion_object::{
    Canonical, Changeset, DeltaEntry, DeltaOp, Extensions, HistoryNode, Meta, RepoPath, Timestamp,
    Tree,
};
use scion_refmodel::{MDelta, MFile, MRepo, MTree};
use scion_store::{
    DerivedKey, DerivedRow, FsckOptions, MemHandles, MemOptions, ObjectRepo, ObjectRepoConfig,
    Storage,
};
use scion_testkit::{GenRepo, encode_tree};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

// ---------------------------------------------------------------------------------------------
// Mock DeriveCtx
// ---------------------------------------------------------------------------------------------

/// An in-memory [`DeriveCtx`]: objects by id, changeset records, stored derived values.
#[derive(Default)]
pub struct MockCtx {
    pub objects: Mutex<HashMap<ObjectId, Bytes>>,
    pub changesets: Mutex<HashMap<ObjectId, ChangesetRecord>>,
    pub stored: Mutex<HashMap<DerivedKey, Bytes>>,
    /// Calls of `put_object`.
    pub puts: AtomicU64,
    /// Calls of `tree`.
    pub tree_reads: AtomicU64,
}

#[async_trait]
impl DeriveCtx for MockCtx {
    async fn changeset(&self, id: &ObjectId) -> Result<ChangesetRecord> {
        self.changesets
            .lock()
            .get(id)
            .cloned()
            .ok_or_else(|| ScionError::not_found(format!("changeset {id}")))
    }

    async fn tree(&self, id: &ObjectId) -> Result<Arc<Tree>> {
        self.tree_reads.fetch_add(1, Ordering::Relaxed);
        let raw = self.get_object(id).await?;
        Ok(Arc::new(Tree::decode(&raw)?))
    }

    async fn meta(&self, _id: &ObjectId) -> Result<Option<Arc<Meta>>> {
        Ok(None)
    }

    async fn blob_or_empty(&self, id: &ObjectId) -> Result<Bytes> {
        Ok(self.objects.lock().get(id).cloned().unwrap_or_default())
    }

    async fn get_object(&self, id: &ObjectId) -> Result<Bytes> {
        self.objects
            .lock()
            .get(id)
            .cloned()
            .ok_or_else(|| ScionError::not_found(format!("object {id}")))
    }

    async fn put_object(&self, kind: Kind, canonical: Bytes) -> Result<ObjectId> {
        self.puts.fetch_add(1, Ordering::Relaxed);
        if kind == history::NODE_KIND {
            HistoryNode::decode(&canonical)?;
        }
        let id = ObjectId::hash_encoded(kind, &canonical);
        self.objects.lock().insert(id, canonical);
        Ok(id)
    }

    async fn stored(&self, key: &DerivedKey) -> Result<Option<Bytes>> {
        Ok(self.stored.lock().get(key).cloned())
    }
}

impl MockCtx {
    pub fn new() -> Arc<Self> {
        Arc::new(MockCtx::default())
    }

    /// Number of stored history nodes.
    pub fn node_count(&self) -> usize {
        self.objects
            .lock()
            .keys()
            .filter(|id| id.kind() == history::NODE_KIND)
            .count()
    }

    /// Store the trees of `t` (blobs are not needed for history derivation; stored anyway).
    pub fn put_tree(&self, t: &MTree) -> ObjectId {
        let (root, objs) = encode_tree(t);
        let mut o = self.objects.lock();
        for (id, _, bytes) in objs {
            o.insert(id, bytes);
        }
        for f in t.values() {
            o.insert(
                ObjectId::hash_blob(&f.content),
                Bytes::from(f.content.clone()),
            );
        }
        root
    }

    /// Commit `tree` on `parents` (whose trees are `parent_trees`, same order): build the
    /// changeset, run [`HistoryType`] through this context, store its rows and the record.
    /// `seq` makes the change id (and thus the changeset id) unique.
    pub async fn commit(
        &self,
        parents: &[ObjectId],
        parent_trees: &[&MTree],
        tree: &MTree,
        seq: u64,
    ) -> (ObjectId, Vec<DerivedRow>) {
        let empty = MTree::new();
        let t1 = parent_trees.first().copied().unwrap_or(&empty);
        let root_tree = self.put_tree(tree);
        let cs = changeset(parents.to_vec(), root_tree, delta_of(t1, tree), seq);
        let id = cs.id();
        let new_trees = new_trees(t1, tree);
        let rows = self.derive(&id, &cs, &new_trees, false).await.unwrap();
        self.store_rows(&rows);
        self.add_record(id, cs);
        (id, rows)
    }

    /// Run [`HistoryType::derive`] for a prepared changeset.
    pub async fn derive(
        &self,
        id: &ObjectId,
        cs: &Changeset,
        new_trees: &[(RepoPath, ObjectId)],
        import_mode: bool,
    ) -> Result<Vec<DerivedRow>> {
        let meta = sample_meta(Extensions::new());
        let input = EagerInput {
            id,
            changeset: cs,
            meta: &meta,
            new_trees,
            import_mode,
        };
        HistoryType.derive(self, &input).await
    }

    pub fn store_rows(&self, rows: &[DerivedRow]) {
        let mut s = self.stored.lock();
        for r in rows {
            // ON CONFLICT DO NOTHING.
            s.entry(r.key.clone())
                .or_insert_with(|| r.value.clone().unwrap());
        }
    }

    pub fn add_record(&self, id: ObjectId, cs: Changeset) {
        self.changesets.lock().insert(
            id,
            ChangesetRecord {
                id,
                changeset: Arc::new(cs),
                generation: 0,
                skip: Vec::new(),
                created_by: "service:test".to_owned(),
            },
        );
    }

    /// Load a generated repository through this context; returns the changeset ids by index.
    pub async fn load(&self, repo: &GenRepo) -> Vec<ObjectId> {
        let mut ids: Vec<ObjectId> = Vec::new();
        for (i, c) in repo.commits.iter().enumerate() {
            let parents: Vec<ObjectId> = c.parents.iter().map(|&p| ids[p]).collect();
            let ptrees: Vec<&MTree> = c.parents.iter().map(|&p| &repo.commits[p].tree).collect();
            let (id, _) = self.commit(&parents, &ptrees, &c.tree, i as u64).await;
            ids.push(id);
        }
        ids
    }
}

// ---------------------------------------------------------------------------------------------
// Structural helpers
// ---------------------------------------------------------------------------------------------

pub fn p(s: &str) -> RepoPath {
    RepoPath::parse(s.as_bytes()).unwrap()
}

pub fn file(content: &str) -> MFile {
    MFile {
        mode: 1,
        content: content.as_bytes().to_vec(),
    }
}

/// A structural tree from `(path, content)` pairs (mode File).
pub fn tree(entries: &[(&str, &str)]) -> MTree {
    entries
        .iter()
        .map(|(p, c)| (p.as_bytes().to_vec(), file(c)))
        .collect()
}

pub fn mode_of(m: u8) -> Mode {
    Mode::from_u8(m).unwrap()
}

/// SPEC 4.3.4 delta between structural trees, as production entries.
pub fn delta_of(a: &MTree, b: &MTree) -> Vec<DeltaEntry> {
    scion_refmodel::file_diff(a, b)
        .into_iter()
        .map(|d| match d {
            MDelta::Put(path, f) => DeltaEntry {
                path: RepoPath::parse(&path).unwrap(),
                op: DeltaOp::Put {
                    mode: mode_of(f.mode),
                    id: ObjectId::hash_blob(&f.content),
                    size: f.content.len() as u64,
                },
            },
            MDelta::Delete(path) => DeltaEntry {
                path: RepoPath::parse(&path).unwrap(),
                op: DeltaOp::Delete,
            },
        })
        .collect()
}

/// Every directory of `t` (root included) with its tree id.
pub fn dir_ids(t: &MTree) -> BTreeMap<Vec<u8>, ObjectId> {
    let mut dirs: BTreeSet<Vec<u8>> = BTreeSet::new();
    dirs.insert(Vec::new());
    for path in t.keys() {
        for (i, &b) in path.iter().enumerate() {
            if b == b'/' {
                dirs.insert(path[..i].to_vec());
            }
        }
    }
    dirs.into_iter()
        .map(|d| {
            let id = ObjectId::from_bytes(&scion_refmodel::enc::tree_id_for_dir(t, &d)).unwrap();
            (d, id)
        })
        .collect()
}

/// What `History::create_changeset` passes as `EagerInput::new_trees`: the non-root directories
/// of `b` whose tree differs from `a` at that path (sorted by path).
pub fn new_trees(a: &MTree, b: &MTree) -> Vec<(RepoPath, ObjectId)> {
    let old = dir_ids(a);
    dir_ids(b)
        .into_iter()
        .filter(|(d, id)| !d.is_empty() && old.get(d) != Some(id))
        .map(|(d, id)| (RepoPath::parse(&d).unwrap(), id))
        .collect()
}

/// Every path of `t`: files, directories and the root.
pub fn all_paths(t: &MTree) -> BTreeSet<Vec<u8>> {
    let mut out: BTreeSet<Vec<u8>> = dir_ids(t).into_keys().collect();
    out.extend(t.keys().cloned());
    out
}

pub fn sample_meta(extensions: Extensions) -> Meta {
    Meta {
        author: b"Test <test@example.com>".to_vec(),
        author_time: Timestamp {
            seconds: 0,
            tz_offset_minutes: 0,
        },
        committer: None,
        committer_time: None,
        creator: b"service:test".to_vec(),
        message: b"test".to_vec(),
        extensions,
    }
}

pub fn change_id(seq: u64) -> ChangeId {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&seq.to_be_bytes());
    b[8] = 0xA5;
    ChangeId(b)
}

/// A changeset value (Meta id synthetic: the mock never reads it).
pub fn changeset(
    parents: Vec<ObjectId>,
    root_tree: ObjectId,
    delta: Vec<DeltaEntry>,
    seq: u64,
) -> Changeset {
    Changeset {
        parents,
        root_tree,
        meta: sample_meta(Extensions::new()).id(),
        change_id: change_id(seq),
        delta,
        extensions: Extensions::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// Node walks
// ---------------------------------------------------------------------------------------------

/// Decoded-node cache over any context (nodes are immutable).
#[derive(Default)]
pub struct NodeCache {
    nodes: HashMap<ObjectId, Arc<HistoryNode>>,
}

impl NodeCache {
    pub async fn get(&mut self, cx: &dyn DeriveCtx, id: &ObjectId) -> Arc<HistoryNode> {
        if let Some(n) = self.nodes.get(id) {
            return n.clone();
        }
        let n = Arc::new(history::get_node(cx, id).await.unwrap());
        self.nodes.insert(*id, n.clone());
        n
    }

    /// The changesets of every node reachable from `start` through node parents
    /// (`first_parent_only` follows only each node's first parent).
    pub async fn reachable_changesets(
        &mut self,
        cx: &dyn DeriveCtx,
        start: ObjectId,
        first_parent_only: bool,
    ) -> BTreeSet<ObjectId> {
        let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
        let mut out = BTreeSet::new();
        let mut queue = VecDeque::from([start]);
        while let Some(n) = queue.pop_front() {
            if !seen.insert(n) {
                continue;
            }
            let node = self.get(cx, &n).await;
            out.insert(node.changeset);
            if first_parent_only {
                queue.extend(node.parents.first().copied());
            } else {
                queue.extend(node.parents.iter().copied());
            }
        }
        out
    }
}

/// The reference model of a generated repository (indices match `repo.commits`).
pub fn model_of(repo: &GenRepo) -> MRepo {
    let mut m = MRepo::new();
    for c in &repo.commits {
        m.commit(&c.parents, c.tree.clone());
    }
    m
}

/// SPEC 19.6: for every commit and every path of it (and of its parents), walking node parents
/// from `node_for_path` gives exactly the commits of `MRepo::log_path` (both with all parents
/// and first-parent only). Returns the number of (commit, path) pairs checked.
pub async fn check_against_model(cx: &dyn DeriveCtx, repo: &GenRepo, ids: &[ObjectId]) -> usize {
    let model = model_of(repo);
    let index: HashMap<ObjectId, usize> = ids.iter().enumerate().map(|(i, c)| (*c, i)).collect();
    assert_eq!(index.len(), ids.len(), "changeset ids are unique");
    let mut cache = NodeCache::default();
    let mut checked = 0;
    for (c, commit) in repo.commits.iter().enumerate() {
        let mut paths = all_paths(&commit.tree);
        for &pi in &commit.parents {
            paths.extend(all_paths(&repo.commits[pi].tree));
        }
        for path in paths {
            let rp = RepoPath::parse(&path).unwrap();
            let node = history::node_for_path(cx, &ids[c], &rp).await.unwrap();
            for first_parent_only in [false, true] {
                let want: BTreeSet<usize> = model
                    .log_path(c, &path, first_parent_only)
                    .into_iter()
                    .collect();
                let got: BTreeSet<usize> = match &node {
                    Some((id, _)) => cache
                        .reachable_changesets(cx, *id, first_parent_only)
                        .await
                        .into_iter()
                        .map(|cs| index[&cs])
                        .collect(),
                    None => BTreeSet::new(),
                };
                assert_eq!(
                    got,
                    want,
                    "commit {c}, path {:?}, first_parent_only {first_parent_only}",
                    String::from_utf8_lossy(&path)
                );
            }
            if let Some((_, n)) = &node {
                // The node's content is the path's content in this commit.
                let expect = match commit.tree.get(&path) {
                    Some(f) => ObjectId::hash_blob(&f.content),
                    None => dir_ids(&commit.tree)[&path],
                };
                assert_eq!(n.content, expect);
            }
            checked += 1;
        }
    }
    checked
}

// ---------------------------------------------------------------------------------------------
// End-to-end stack: History + DerivedEngine (Registry::v1_default) as the EagerHook
// ---------------------------------------------------------------------------------------------

pub struct Stack {
    pub h: History,
    pub engine: Arc<DerivedEngine>,
    pub mem: MemHandles,
    pub clock: Arc<ManualClock>,
}

pub fn stack() -> Stack {
    let (storage, mem) = Storage::in_memory(MemOptions::default());
    let clock = ManualClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    let objects = ObjectRepo::new(storage, ObjectRepoConfig::default()).with_clock(clock.clone());
    let engine = DerivedEngine::new(
        objects.clone(),
        Registry::v1_default(),
        DeriveConfig::default(),
        clock.clone(),
    );
    let h = History::new(
        objects,
        HistoryConfig::default(),
        engine.clone(),
        clock.clone(),
    );
    Stack {
        h,
        engine,
        mem,
        clock,
    }
}

pub fn alice() -> Principal {
    Principal::GitHub {
        login: "alice".to_owned(),
        id: Some(7),
    }
}

pub fn importer() -> Principal {
    Principal::Service("importer".to_owned())
}

pub fn fields(message: &str, extensions: Extensions) -> MetaFields {
    MetaFields {
        author: b"Alice <alice@example.com>".to_vec(),
        author_time: Timestamp {
            seconds: 1_700_000_000,
            tz_offset_minutes: 0,
        },
        committer: None,
        committer_time: None,
        message: message.as_bytes().to_vec(),
        extensions,
    }
}

impl Stack {
    /// Upload every file of `t`.
    pub async fn upload(&self, t: &MTree) {
        for f in t.values() {
            self.h
                .objects()
                .put_blob(Bytes::from(f.content.clone()))
                .await
                .unwrap();
        }
    }

    /// Create a changeset whose tree is `tree` on `parents` (first parent's tree `t1`).
    pub async fn commit_tree(
        &self,
        parents: Vec<ObjectId>,
        t1: &MTree,
        tree: &MTree,
        seq: u64,
        import_mode: bool,
        extensions: Extensions,
    ) -> Result<ObjectId> {
        self.upload(tree).await;
        let req = CreateRequest {
            parents,
            delta: delta_of(t1, tree),
            meta: fields(&format!("commit {seq}"), extensions),
            change_id: change_id(seq),
            expected_root_tree: None,
            import_mode,
        };
        let who = if import_mode { importer() } else { alice() };
        Ok(self
            .h
            .create_changeset(req, &who, &AllowAll)
            .await?
            .changeset_id)
    }

    /// Load a generated repository (commit 0 parentless in import mode); ids by index.
    pub async fn load(&self, repo: &GenRepo) -> Vec<ObjectId> {
        let empty = MTree::new();
        let mut ids: Vec<ObjectId> = Vec::new();
        for (i, c) in repo.commits.iter().enumerate() {
            let parents: Vec<ObjectId> = c.parents.iter().map(|&p| ids[p]).collect();
            let t1 = c
                .parents
                .first()
                .map(|&p| &repo.commits[p].tree)
                .unwrap_or(&empty);
            let id = self
                .commit_tree(
                    parents,
                    t1,
                    &c.tree,
                    i as u64,
                    c.parents.is_empty(),
                    Extensions::new(),
                )
                .await
                .unwrap();
            ids.push(id);
        }
        ids
    }

    /// fsck with every eager type the engine requires plus every byte (SPEC 4.5).
    pub async fn fsck(&self) -> Vec<scion_store::Violation> {
        let opts = FsckOptions {
            required_eager: scion_history::EagerHook::required_eager(self.engine.as_ref()),
            verify_bytes: true,
        };
        scion_store::fsck(self.h.objects(), &opts).await.unwrap()
    }

    /// Stored history nodes (rows of kind 0x10).
    pub fn node_rows(&self) -> usize {
        self.mem
            .meta
            .all_objects()
            .iter()
            .filter(|r| r.kind == history::NODE_KIND)
            .count()
    }
}

/// Is `e` the error code `code`?
pub fn is_code<T: std::fmt::Debug>(r: &Result<T>, code: ErrorCode) -> bool {
    matches!(r, Err(e) if e.code == code)
}

pub fn empty_tree() -> ObjectId {
    empty_tree_id()
}
