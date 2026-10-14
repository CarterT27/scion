//! SPEC 19.18 gate: on shared-subtree repositories of 10^3 and 10^5 files (10^7 in the ignored
//! `full_` variant) with equal depth, eager derivation of a 1-file change creates at most
//! `depth + 1` history nodes, counted via `objects_written` of the derived kind, independent of
//! repository size. History runs with `DerivedEngine` (`Registry::v1_default`) as the EagerHook.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod history_support;

use bytes::Bytes;
use history_support::*;
use scion_core::{ChangeId, Kind, Mode, ObjectId};
use scion_derived::history;
use scion_history::{AllowAll, CreateRequest};
use scion_object::{Canonical, Changeset, DeltaEntry, DeltaOp, Extensions, HistoryNode, NodeKind};
use scion_store::{ChangesetBundle, ChangesetRow, CounterSnapshot, ObjectRepo};
use scion_testkit::shared_subtree_repo;
use std::collections::HashMap;

/// Depth (path components of every file) used for every size.
const DEPTH: u32 = 5;

/// Store the history nodes of a parentless changeset `cs` whose tree is `root` (every node is
/// new and has no parents). Identical subtrees give identical nodes, so this is
/// O(distinct objects) work. Returns the root node id.
async fn parentless_nodes(objects: &ObjectRepo, cs: ObjectId, root: ObjectId) -> ObjectId {
    let mut memo: HashMap<ObjectId, ObjectId> = HashMap::new();
    // Post-order over distinct trees with an explicit stack.
    let mut stack: Vec<(ObjectId, bool)> = vec![(root, false)];
    while let Some((t, expanded)) = stack.pop() {
        if memo.contains_key(&t) {
            continue;
        }
        let tree = objects.get_tree(&t).await.unwrap();
        if !expanded {
            stack.push((t, true));
            for e in &tree.entries {
                if e.mode == Mode::Tree && !memo.contains_key(&e.id) {
                    stack.push((e.id, false));
                }
            }
            continue;
        }
        let mut children = Vec::with_capacity(tree.entries.len());
        for e in &tree.entries {
            let child = if e.mode == Mode::Tree {
                memo[&e.id]
            } else {
                match memo.get(&e.id) {
                    Some(n) => *n,
                    None => {
                        let n = put_node(objects, NodeKind::File, e.id, cs, Vec::new()).await;
                        memo.insert(e.id, n);
                        n
                    }
                }
            };
            children.push((e.name.clone(), child));
        }
        let n = put_node(objects, NodeKind::Dir, t, cs, children).await;
        memo.insert(t, n);
    }
    memo[&root]
}

async fn put_node(
    objects: &ObjectRepo,
    kind: NodeKind,
    content: ObjectId,
    cs: ObjectId,
    children: Vec<(scion_object::Name, ObjectId)>,
) -> ObjectId {
    let node = HistoryNode {
        kind,
        content,
        changeset: cs,
        parents: Vec::new(),
        children,
        extensions: Extensions::new(),
    };
    objects
        .put_encoded(history::NODE_KIND, Bytes::from(node.encode()))
        .await
        .unwrap()
        .0
}

struct GateResult {
    depth: u64,
    counters: CounterSnapshot,
    new_nodes: u64,
}

/// Load the shared-subtree repository as a parentless fixture changeset (with its history
/// row), then run one 1-file CreateChangeset and measure it.
async fn one_file_change(files: u64) -> GateResult {
    let repo = shared_subtree_repo(files, DEPTH);
    let s = stack();
    let objects = s.h.objects().clone();
    for (id, kind, bytes) in repo.encoded_objects() {
        let stored = match kind {
            Kind::Blob => objects.put_blob(bytes).await.unwrap().0,
            k => objects.put_encoded(k, bytes).await.unwrap().0,
        };
        assert_eq!(stored, id);
    }
    let meta = sample_meta(Extensions::new());
    let (meta_id, _) = objects
        .put_encoded(Kind::Meta, Bytes::from(meta.encode()))
        .await
        .unwrap();
    let base = Changeset {
        parents: Vec::new(),
        root_tree: repo.root_tree(),
        meta: meta_id,
        change_id: ChangeId([7; 16]),
        delta: Vec::new(),
        extensions: Extensions::new(),
    };
    let encoded = Bytes::from(base.encode());
    let base_id = ObjectId::hash_encoded(Kind::Changeset, &encoded);
    let root_node = parentless_nodes(&objects, base_id, repo.root_tree()).await;
    let row = scion_store::DerivedRow {
        key: history::derived_key(&base_id),
        value: Some(Bytes::copy_from_slice(root_node.as_bytes())),
        storage_key: None,
        source_object: None,
        at: std::time::SystemTime::UNIX_EPOCH,
    };
    s.h.meta()
        .commit_changeset(ChangesetBundle {
            row: ChangesetRow {
                id: base_id,
                encoded,
                generation: 1,
                skip: Vec::new(),
                change_id: base.change_id,
                created_by: "service:fixture".to_owned(),
                created_at: std::time::SystemTime::UNIX_EPOCH,
            },
            parents: Vec::new(),
            object_paths: Vec::new(),
            derived: vec![row],
        })
        .await
        .unwrap();

    let content = b"perturbed content\n".to_vec();
    let (blob, _) = objects
        .put_blob(Bytes::from(content.clone()))
        .await
        .unwrap();
    let path = repo.perturb_path();
    let req = CreateRequest {
        parents: vec![base_id],
        delta: vec![DeltaEntry {
            path: path.clone(),
            op: DeltaOp::Put {
                mode: Mode::File,
                id: blob,
                size: content.len() as u64,
            },
        }],
        meta: fields("perturb", Extensions::new()),
        change_id: ChangeId([8; 16]),
        expected_root_tree: None,
        import_mode: false,
    };
    let nodes_before = s.node_rows();
    let counters = objects.storage().counters.clone();
    let before = counters.snapshot();
    let created =
        s.h.create_changeset(req, &alice(), &AllowAll)
            .await
            .unwrap();
    let diff = counters.snapshot() - before;
    let new_nodes = (s.node_rows() - nodes_before) as u64;

    // The new node of the perturbed file links the fixture's node.
    let (_, n) = history::node_for_path(s.engine.ctx(), &created.changeset_id, &path)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n.changeset, created.changeset_id);
    assert_eq!(n.parents.len(), 1);
    GateResult {
        depth: u64::from(repo.depth),
        counters: diff,
        new_nodes,
    }
}

async fn gate(files: u64) -> GateResult {
    let g = one_file_change(files).await;
    let d = &g.counters;
    // Objects written by the create: trees, one Meta, and the derived (history) nodes.
    let derived_written = d.objects_written - d.trees_written - 1;
    eprintln!(
        "19.18 history gate: {files} files, depth {}: history nodes written {derived_written}, \
         trees_read {}, trees_written {}, derived_read {}",
        g.depth, d.trees_read, d.trees_written, d.derived_read
    );
    assert_eq!(
        derived_written, g.new_nodes,
        "objects_written of the derived kind"
    );
    assert!(
        derived_written <= g.depth + 1,
        "{files} files: {derived_written} history nodes (depth {})",
        g.depth
    );
    // History derivation reads no trees: CreateChangeset's tree budget is unchanged.
    assert!(d.trees_read <= g.depth + 2, "trees_read {}", d.trees_read);
    assert_eq!(d.blobs_read, 0);
    g
}

#[tokio::test]
async fn gate_1e3_and_1e5_are_size_independent() {
    let small = gate(1_000).await;
    let large = gate(100_000).await;
    assert_eq!(small.new_nodes, large.new_nodes);
    assert_eq!(small.new_nodes, small.depth + 1);
    assert_eq!(
        small.counters.objects_written,
        large.counters.objects_written
    );
    assert_eq!(small.counters.derived_read, large.counters.derived_read);
}

#[tokio::test]
#[ignore = "spec-sized (SCION_FULL): 10^7 files"]
async fn full_gate_1e7() {
    if !scion_core::test_support::full_mode() {
        eprintln!("SKIP: SCION_FULL unset");
        return;
    }
    let small = gate(1_000).await;
    let huge = gate(10_000_000).await;
    assert_eq!(small.new_nodes, huge.new_nodes);
    assert_eq!(
        small.counters.objects_written,
        huge.counters.objects_written
    );
}
