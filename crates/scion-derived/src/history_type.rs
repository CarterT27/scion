//! SPEC 9.2 `history` (eager): history nodes of derived kind 0x10 (P21).
//!
//! Every path of every changeset `C` has a history node
//! `HistoryNode { kind, content, changeset, parents, children (Dir), extensions }`:
//!
//! - **Reuse.** When the path's content in `C` (blob id for a file, tree id for a directory)
//!   equals its content in `parents[0]`, the node is that parent's node.
//! - **New node.** Otherwise a node is created with `changeset = C`, `parents` = the nodes of the
//!   path in each parent of `C` where the path exists (deduplicated, parent order), and for a
//!   directory `children` = `(name, node)` for every entry, unchanged entries reusing the
//!   first parent's child nodes.
//!
//! Only the delta paths and their ancestor directories can differ from `parents[0]` (the delta
//! is `FileDiff(T1, root_tree)`, SPEC 4.3.4), so a changeset creates O(|delta| x depth) nodes
//! and reads no tree: directory contents come from `EagerInput::new_trees`, child lists from
//! the first parent's nodes.
//!
//! The per-changeset derived row is `key = <34 raw changeset id bytes>`, `value = <34 raw root
//! node id bytes>`. The root changeset with an empty tree gets a root `Dir` node without
//! children.
//!
//! A parent whose history row is absent (for example a history built without eager
//! derivation) is derived first: the missing ancestors are walked iteratively (never by
//! recursion), derived in topological order from their stored trees, and their rows are
//! returned together with the new changeset's row (`ON CONFLICT DO NOTHING` makes that
//! harmless). Derivation is deterministic: identical inputs give byte-identical rows.

use crate::types::{DeriveCtx, DeriveMode, DerivedType, EagerType};
use async_trait::async_trait;
use bytes::Bytes;
use scion_core::{ErrorCode, Kind, Mode, ObjectId, Result, ScionError, empty_tree_id, subcode};
use scion_history::EagerInput;
use scion_object::{
    Canonical, Changeset, DeltaOp, Extensions, HistoryNode, Name, NodeKind, RepoPath,
};
use scion_store::{DerivedKey, DerivedRow};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;

/// Type name.
pub const NAME: &str = "history";
/// Type version.
pub const VERSION: u32 = 1;

/// The derived kind of history nodes (SPEC 4.2, 9.2).
pub const NODE_KIND: Kind = Kind::Derived(0x10);

/// The eager history-node type (SPEC 9.2; not content-bearing).
#[derive(Clone, Copy, Debug, Default)]
pub struct HistoryType;

impl DerivedType for HistoryType {
    fn name(&self) -> &'static str {
        NAME
    }

    fn version(&self) -> u32 {
        VERSION
    }

    fn mode(&self) -> DeriveMode {
        DeriveMode::Eager
    }

    fn content_bearing(&self) -> bool {
        false
    }
}

#[async_trait]
impl EagerType for HistoryType {
    /// SPEC 9.2: the nodes of every path whose content changed versus `parents[0]` are written
    /// as objects of kind 0x10; the rows are this changeset's root node, plus the rows of any
    /// ancestor whose history row was missing (derived on the way).
    async fn derive(&self, cx: &dyn DeriveCtx, input: &EagerInput<'_>) -> Result<Vec<DerivedRow>> {
        let mut d = Deriver::new(cx);
        let roots = d.roots_of(&input.changeset.parents).await?;
        let new_trees: BTreeMap<RepoPath, ObjectId> = input.new_trees.iter().cloned().collect();
        let root = d
            .derive_nodes(input.id, input.changeset, &new_trees, &roots)
            .await?;
        let mut rows = vec![root_row(input.id, &root)];
        rows.extend(d.backfilled.iter().map(|(cs, n)| root_row(cs, n)));
        tracing::debug!(
            changeset = %input.id,
            nodes_written = d.written.len(),
            backfilled = d.backfilled.len(),
            "history nodes derived"
        );
        Ok(rows)
    }
}

/// The derived key of changeset `cs`'s history row (SPEC 9.2 per-changeset value).
pub fn derived_key(cs: &ObjectId) -> DerivedKey {
    DerivedKey {
        ty: NAME.to_owned(),
        version: VERSION,
        key: Bytes::copy_from_slice(cs.as_bytes()),
    }
}

/// The row `cs -> root node`. Eager rows carry no wall-clock time so that derivation stays
/// deterministic (SPEC 9.1); the changesets row records the creation time.
fn root_row(cs: &ObjectId, root: &ObjectId) -> DerivedRow {
    DerivedRow {
        key: derived_key(cs),
        value: Some(Bytes::copy_from_slice(root.as_bytes())),
        storage_key: None,
        source_object: None,
        at: SystemTime::UNIX_EPOCH,
    }
}

/// The root history node id of changeset `cs` (SPEC 9.2 per-changeset value).
///
/// Reads the stored row; when it is absent (a changeset created without eager derivation) the
/// value is derived from the stored history (node objects are written, rows are not).
/// NOT_FOUND when the changeset does not exist.
pub async fn root_node(cx: &dyn DeriveCtx, cs: &ObjectId) -> Result<ObjectId> {
    let mut d = Deriver::new(cx);
    let roots = d.roots_of(std::slice::from_ref(cs)).await?;
    roots
        .into_iter()
        .next()
        .ok_or_else(|| ScionError::not_found(format!("no history node for {cs}")))
}

/// The node for `path` in changeset `cs`, if the path exists (SPEC 9.2): walks the children
/// from the root node.
pub async fn node_for_path(
    cx: &dyn DeriveCtx,
    cs: &ObjectId,
    path: &RepoPath,
) -> Result<Option<(ObjectId, HistoryNode)>> {
    let root = root_node(cx, cs).await?;
    let mut d = Deriver::new(cx);
    match d.lookup(root, path).await? {
        Some(id) => {
            let node = d.node(&id).await?;
            Ok(Some((id, (*node).clone())))
        }
        None => Ok(None),
    }
}

/// Decode the node `id` (SPEC 9.2). INVALID_ARGUMENT/invalid_kind when `id` is not of kind
/// 0x10.
pub async fn get_node(cx: &dyn DeriveCtx, id: &ObjectId) -> Result<HistoryNode> {
    check_node_id(id)?;
    let raw = cx.get_object(id).await?;
    decode_node(id, &raw)
}

fn check_node_id(id: &ObjectId) -> Result<()> {
    if id.kind() != NODE_KIND {
        return Err(ScionError::invalid(
            subcode::INVALID_KIND,
            format!("{id} is not a history node id"),
        ));
    }
    Ok(())
}

fn decode_node(id: &ObjectId, raw: &[u8]) -> Result<HistoryNode> {
    let node = HistoryNode::decode(raw)?;
    if ObjectId::hash_encoded(NODE_KIND, raw) != *id {
        return Err(ScionError::integrity(id));
    }
    Ok(node)
}

/// An inconsistent derivation input (the eager input disagrees with the delta or the stored
/// history).
fn inconsistent(m: impl Into<String>) -> ScionError {
    ScionError::new(
        ErrorCode::InvalidArgument,
        format!("history derivation: {}", m.into()),
    )
}

/// The state of one path of the new changeset that may differ from `parents[0]`.
#[derive(Clone, Copy, Debug)]
enum Target {
    /// A file with this blob.
    File(ObjectId),
    /// A directory with this tree.
    Dir(ObjectId),
    /// Absent from the new tree.
    Gone,
}

/// Number of components of `p` (root = 0).
fn depth(p: &RepoPath) -> usize {
    p.components().count()
}

/// One derivation: decoded-node and path caches plus the set of nodes already written.
struct Deriver<'a> {
    cx: &'a dyn DeriveCtx,
    /// Decoded nodes by id (immutable objects).
    nodes: HashMap<ObjectId, Arc<HistoryNode>>,
    /// `(root node, path) -> node at path`.
    paths: HashMap<(ObjectId, RepoPath), Option<ObjectId>>,
    /// `(root tree, path) -> (mode, id)` of the tree entry at path.
    entries: HashMap<(ObjectId, RepoPath), Option<(Mode, ObjectId)>>,
    /// Node ids stored during this derivation.
    written: HashSet<ObjectId>,
    /// Ancestors whose missing history row was derived: `(changeset, root node)`.
    backfilled: Vec<(ObjectId, ObjectId)>,
}

impl<'a> Deriver<'a> {
    fn new(cx: &'a dyn DeriveCtx) -> Self {
        Deriver {
            cx,
            nodes: HashMap::new(),
            paths: HashMap::new(),
            entries: HashMap::new(),
            written: HashSet::new(),
            backfilled: Vec::new(),
        }
    }

    /// A decoded node (cached).
    async fn node(&mut self, id: &ObjectId) -> Result<Arc<HistoryNode>> {
        if let Some(n) = self.nodes.get(id) {
            return Ok(n.clone());
        }
        let n = Arc::new(get_node(self.cx, id).await?);
        self.nodes.insert(*id, n.clone());
        Ok(n)
    }

    /// Store `node` (once per derivation) and return its id.
    async fn put(&mut self, node: HistoryNode) -> Result<ObjectId> {
        let bytes = node.encode();
        let id = ObjectId::hash_encoded(NODE_KIND, &bytes);
        if self.written.insert(id) {
            let stored = self.cx.put_object(NODE_KIND, Bytes::from(bytes)).await?;
            if stored != id {
                return Err(ScionError::integrity(&id));
            }
            self.nodes.insert(id, Arc::new(node));
        }
        Ok(id)
    }

    /// The node at `path` below the root node `root` (walks children; cached per prefix).
    async fn lookup(&mut self, root: ObjectId, path: &RepoPath) -> Result<Option<ObjectId>> {
        let mut prefixes = path.ancestors();
        prefixes.push(path.clone());
        let mut cur = root;
        for p in prefixes.into_iter().filter(|p| !p.is_root()) {
            let key = (root, p);
            let next = match self.paths.get(&key) {
                Some(v) => *v,
                None => {
                    let node = self.node(&cur).await?;
                    let child = key.1.file_name().and_then(|n| {
                        node.children
                            .binary_search_by(|(c, _)| c.as_bytes().cmp(n))
                            .ok()
                            .and_then(|i| node.children.get(i))
                            .map(|(_, id)| *id)
                    });
                    self.paths.insert(key, child);
                    child
                }
            };
            match next {
                Some(id) => cur = id,
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    /// The root nodes of `changesets`, in order. Changesets without a stored history row are
    /// derived first (iteratively, parents before children); their rows are recorded in
    /// `self.backfilled`.
    async fn roots_of(&mut self, changesets: &[ObjectId]) -> Result<Vec<ObjectId>> {
        let mut known: HashMap<ObjectId, ObjectId> = HashMap::new();
        let mut records: HashMap<ObjectId, Arc<Changeset>> = HashMap::new();
        // (changeset, parents expanded)
        let mut stack: Vec<(ObjectId, bool)> = Vec::new();
        for cs in changesets {
            if !known.contains_key(cs) {
                match stored_root(self.cx, cs).await? {
                    Some(r) => {
                        known.insert(*cs, r);
                    }
                    None => stack.push((*cs, false)),
                }
            }
        }
        while let Some((cs, expanded)) = stack.pop() {
            if known.contains_key(&cs) {
                continue;
            }
            if !expanded {
                let rec = self.cx.changeset(&cs).await?;
                stack.push((cs, true));
                for p in &rec.changeset.parents {
                    if known.contains_key(p) {
                        continue;
                    }
                    match stored_root(self.cx, p).await? {
                        Some(r) => {
                            known.insert(*p, r);
                        }
                        None => stack.push((*p, false)),
                    }
                }
                records.insert(cs, rec.changeset);
                continue;
            }
            let changeset = records
                .get(&cs)
                .cloned()
                .ok_or_else(|| inconsistent(format!("changeset {cs} vanished during backfill")))?;
            let mut parent_roots = Vec::with_capacity(changeset.parents.len());
            for p in &changeset.parents {
                let r = known.get(p).copied().ok_or_else(|| {
                    inconsistent(format!("parent {p} of {cs} has no history node"))
                })?;
                parent_roots.push(r);
            }
            let t1 = match changeset.parents.first() {
                Some(p0) => match records.get(p0) {
                    Some(c) => c.root_tree,
                    None => self.cx.changeset(p0).await?.changeset.root_tree,
                },
                None => empty_tree_id(),
            };
            let new_trees = self.new_trees_of(&changeset, t1).await?;
            let root = self
                .derive_nodes(&cs, &changeset, &new_trees, &parent_roots)
                .await?;
            tracing::debug!(changeset = %cs, "history row backfilled");
            known.insert(cs, root);
            self.backfilled.push((cs, root));
        }
        changesets
            .iter()
            .map(|cs| {
                known
                    .get(cs)
                    .copied()
                    .ok_or_else(|| inconsistent(format!("no history node for {cs}")))
            })
            .collect()
    }

    /// The tree entry `(mode, id)` at `path` below the root tree `root` (cached per prefix).
    async fn entry_at(
        &mut self,
        root: ObjectId,
        path: &RepoPath,
    ) -> Result<Option<(Mode, ObjectId)>> {
        let mut prefixes = path.ancestors();
        prefixes.push(path.clone());
        let mut cur = (Mode::Tree, root);
        for p in prefixes.into_iter().filter(|p| !p.is_root()) {
            let key = (root, p);
            let next = match self.entries.get(&key) {
                Some(v) => *v,
                None => {
                    let found = if cur.0 == Mode::Tree {
                        let tree = self.cx.tree(&cur.1).await?;
                        key.1
                            .file_name()
                            .and_then(|n| tree.get(n))
                            .map(|e| (e.mode, e.id))
                    } else {
                        None
                    };
                    self.entries.insert(key, found);
                    found
                }
            };
            match next {
                Some(e) => cur = e,
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    /// The directories of `cs`'s tree that differ from `t1` at their path (what
    /// `EagerInput::new_trees` carries at creation), computed from the stored trees.
    async fn new_trees_of(
        &mut self,
        cs: &Changeset,
        t1: ObjectId,
    ) -> Result<BTreeMap<RepoPath, ObjectId>> {
        let mut dirs: Vec<RepoPath> = cs
            .delta
            .iter()
            .flat_map(|e| e.path.ancestors())
            .filter(|p| !p.is_root())
            .collect();
        dirs.sort();
        dirs.dedup();
        let mut out = BTreeMap::new();
        for d in dirs {
            if let Some((Mode::Tree, id)) = self.entry_at(cs.root_tree, &d).await?
                && self.entry_at(t1, &d).await? != Some((Mode::Tree, id))
            {
                out.insert(d, id);
            }
        }
        Ok(out)
    }

    /// Derive the nodes of changeset `cs` (SPEC 9.2) and return its root node.
    ///
    /// `new_trees` maps every non-root directory whose tree differs from `parents[0]` to its new
    /// tree id; `parent_roots` are the root nodes of `cs.parents`, in order.
    async fn derive_nodes(
        &mut self,
        cs_id: &ObjectId,
        cs: &Changeset,
        new_trees: &BTreeMap<RepoPath, ObjectId>,
        parent_roots: &[ObjectId],
    ) -> Result<ObjectId> {
        let p0 = parent_roots.first().copied();
        if let Some(r0) = p0
            && self.node(&r0).await?.content == cs.root_tree
        {
            // The whole tree equals the first parent's: every node is reused.
            return Ok(r0);
        }

        // Every path that may differ from parents[0]: the delta paths, the changed
        // directories, and their ancestors. A deleted file replaced by a directory is
        // overridden by its `new_trees` entry.
        let mut targets: BTreeMap<RepoPath, Target> = BTreeMap::new();
        targets.insert(RepoPath::root(), Target::Dir(cs.root_tree));
        for e in &cs.delta {
            for a in e.path.ancestors() {
                targets.entry(a).or_insert(Target::Gone);
            }
            let t = match &e.op {
                DeltaOp::Put { id, .. } => Target::File(*id),
                DeltaOp::Delete => Target::Gone,
            };
            targets.insert(e.path.clone(), t);
        }
        for (p, tree) in new_trees {
            if p.is_root() {
                continue;
            }
            for a in p.ancestors() {
                targets.entry(a).or_insert(Target::Gone);
            }
            if let Some(Target::File(_)) = targets.get(p) {
                return Err(inconsistent(format!(
                    "{} is both a file and a directory",
                    String::from_utf8_lossy(p.as_bytes())
                )));
            }
            targets.insert(p.clone(), Target::Dir(*tree));
        }

        // Deepest first, so every child is settled before its directory.
        let mut order: Vec<(RepoPath, Target)> = targets.into_iter().collect();
        order.sort_by_key(|(p, _)| Reverse(depth(p)));

        // Settled children of each directory: (name, Some(node)) or (name, None) when gone.
        let mut updates: HashMap<RepoPath, Vec<(Name, Option<ObjectId>)>> = HashMap::new();
        let mut root: Option<ObjectId> = None;
        for (p, target) in order {
            let pending = updates.remove(&p).unwrap_or_default();
            let settled = match target {
                Target::Gone => {
                    if pending.iter().any(|(_, n)| n.is_some()) {
                        return Err(inconsistent(format!(
                            "{} has entries but is not a directory of the new tree",
                            String::from_utf8_lossy(p.as_bytes())
                        )));
                    }
                    None
                }
                Target::File(blob) => {
                    if pending.iter().any(|(_, n)| n.is_some()) {
                        return Err(inconsistent(format!(
                            "file {} has entries",
                            String::from_utf8_lossy(p.as_bytes())
                        )));
                    }
                    Some(
                        self.settle(cs_id, &p, NodeKind::File, blob, Vec::new(), parent_roots)
                            .await?,
                    )
                }
                Target::Dir(tree) => Some(
                    self.settle(cs_id, &p, NodeKind::Dir, tree, pending, parent_roots)
                        .await?,
                ),
            };
            match (p.parent(), p.file_name()) {
                (Some(parent), Some(name)) => {
                    let name = Name::new(name)?;
                    updates.entry(parent).or_default().push((name, settled));
                }
                _ => root = settled,
            }
        }
        root.ok_or_else(|| inconsistent("no root node"))
    }

    /// The node of path `p` with `content` in changeset `cs_id`: the first parent's node when
    /// its content is equal, else a new node (SPEC 9.2).
    async fn settle(
        &mut self,
        cs_id: &ObjectId,
        p: &RepoPath,
        kind: NodeKind,
        content: ObjectId,
        child_updates: Vec<(Name, Option<ObjectId>)>,
        parent_roots: &[ObjectId],
    ) -> Result<ObjectId> {
        let n0 = match parent_roots.first() {
            Some(r0) => self.lookup(*r0, p).await?,
            None => None,
        };
        let n0_node = match &n0 {
            Some(id) => Some(self.node(id).await?),
            None => None,
        };
        if let (Some(id), Some(node)) = (n0, &n0_node)
            && node.content == content
        {
            return Ok(id);
        }
        let mut parents: Vec<ObjectId> = Vec::with_capacity(parent_roots.len());
        for (i, r) in parent_roots.iter().enumerate() {
            let x = if i == 0 {
                n0
            } else {
                self.lookup(*r, p).await?
            };
            if let Some(x) = x
                && !parents.contains(&x)
            {
                parents.push(x);
            }
        }
        let children = match kind {
            NodeKind::File => Vec::new(),
            NodeKind::Dir => {
                let mut map: BTreeMap<Name, ObjectId> = match &n0_node {
                    Some(n) if n.kind == NodeKind::Dir => n.children.iter().cloned().collect(),
                    _ => BTreeMap::new(),
                };
                for (name, v) in child_updates {
                    match v {
                        Some(id) => {
                            map.insert(name, id);
                        }
                        None => {
                            map.remove(&name);
                        }
                    }
                }
                map.into_iter().collect()
            }
        };
        self.put(HistoryNode {
            kind,
            content,
            changeset: *cs_id,
            parents,
            children,
            extensions: Extensions::new(),
        })
        .await
    }
}

/// The stored root node of `cs`, if its history row exists.
async fn stored_root(cx: &dyn DeriveCtx, cs: &ObjectId) -> Result<Option<ObjectId>> {
    match cx.stored(&derived_key(cs)).await? {
        Some(v) => {
            let id = ObjectId::from_bytes(&v)?;
            check_node_id(&id)?;
            Ok(Some(id))
        }
        None => Ok(None),
    }
}
