//! SPEC 9.2 history nodes derived through a mock `DeriveCtx` (an in-memory map): node shape,
//! the reuse rule (including merges), deletions, file/directory transitions, determinism,
//! backfill of ancestors without a history row, and inconsistent input.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod history_support;

use history_support::*;
use scion_core::{ErrorCode, Kind, ObjectId, subcode};
use scion_derived::history::{self, HistoryType};
use scion_derived::{DeriveMode, DerivedType};
use scion_object::{Changeset, DeltaEntry, DeltaOp, HistoryNode, NodeKind};
use scion_refmodel::{MFile, MTree};
use std::sync::atomic::Ordering;

async fn node(cx: &MockCtx, cs: &ObjectId, path: &str) -> Option<(ObjectId, HistoryNode)> {
    history::node_for_path(cx, cs, &p(path)).await.unwrap()
}

async fn node_id(cx: &MockCtx, cs: &ObjectId, path: &str) -> ObjectId {
    node(cx, cs, path).await.expect("path exists").0
}

#[test]
fn descriptor() {
    assert_eq!(HistoryType.name(), "history");
    assert_eq!(HistoryType.version(), 1);
    assert_eq!(HistoryType.mode(), DeriveMode::Eager);
    assert!(!HistoryType.content_bearing());
    assert_eq!(history::NODE_KIND, Kind::Derived(0x10));
}

#[tokio::test]
async fn root_changeset_with_empty_tree_has_a_childless_dir_root() {
    let cx = MockCtx::new();
    let (c0, rows) = cx.commit(&[], &[], &MTree::new(), 0).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.key, history::derived_key(&c0));
    assert_eq!(&row.key.key[..], c0.as_bytes());
    assert!(row.source_object.is_none());
    let root = ObjectId::from_bytes(row.value.as_ref().unwrap()).unwrap();
    assert_eq!(root.kind(), Kind::Derived(0x10));
    assert_eq!(history::root_node(cx.as_ref(), &c0).await.unwrap(), root);
    let n = history::get_node(cx.as_ref(), &root).await.unwrap();
    assert_eq!(n.kind, NodeKind::Dir);
    assert_eq!(n.content, empty_tree());
    assert_eq!(n.changeset, c0);
    assert!(n.parents.is_empty());
    assert!(n.children.is_empty());
    assert!(n.extensions.iter().next().is_none());
    // The root path resolves to the root node; any other path is absent.
    assert_eq!(node_id(&cx, &c0, "").await, root);
    assert!(node(&cx, &c0, "a").await.is_none());
    assert_eq!(cx.node_count(), 1);
}

#[tokio::test]
async fn nodes_are_created_only_for_changed_paths_and_reused_otherwise() {
    let cx = MockCtx::new();
    let t0 = MTree::new();
    let (c0, _) = cx.commit(&[], &[], &t0, 0).await;
    let t1 = tree(&[("a/b/x", "x1"), ("a/y", "y1"), ("z", "z1")]);
    let (c1, _) = cx.commit(&[c0], &[&t0], &t1, 1).await;
    // Every path of c1 is new: a/b/x, a/b, a/y, a, z and the root.
    assert_eq!(cx.node_count(), 1 + 6);
    let (_, x) = node(&cx, &c1, "a/b/x").await.unwrap();
    assert_eq!(x.kind, NodeKind::File);
    assert_eq!(x.content, ObjectId::hash_blob(b"x1"));
    assert_eq!(x.changeset, c1);
    assert!(x.parents.is_empty(), "a/b/x did not exist in c0");
    assert!(x.children.is_empty());
    let (r1, root1) = node(&cx, &c1, "").await.unwrap();
    assert_eq!(root1.parents, vec![node_id(&cx, &c0, "").await]);
    let names: Vec<&[u8]> = root1.children.iter().map(|(n, _)| n.as_bytes()).collect();
    assert_eq!(names, vec![&b"a"[..], b"z"]);
    let (_, a) = node(&cx, &c1, "a").await.unwrap();
    assert_eq!(a.kind, NodeKind::Dir);
    assert_eq!(a.content, dir_ids(&t1)[&b"a".to_vec()]);

    // c2 changes a/y only: a/y, a and the root are new; a/b, a/b/x and z are reused.
    let before = cx.node_count();
    let t2 = tree(&[("a/b/x", "x1"), ("a/y", "y2"), ("z", "z1")]);
    let (c2, _) = cx.commit(&[c1], &[&t1], &t2, 2).await;
    assert_eq!(cx.node_count() - before, 3);
    for unchanged in ["a/b", "a/b/x", "z"] {
        assert_eq!(
            node_id(&cx, &c2, unchanged).await,
            node_id(&cx, &c1, unchanged).await,
            "{unchanged}"
        );
    }
    let (_, y2) = node(&cx, &c2, "a/y").await.unwrap();
    assert_eq!(y2.changeset, c2);
    assert_eq!(y2.parents, vec![node_id(&cx, &c1, "a/y").await]);
    let (_, root2) = node(&cx, &c2, "").await.unwrap();
    assert_eq!(root2.parents, vec![r1]);
    // Unchanged children of a changed directory point at the reused nodes.
    let (_, a2) = node(&cx, &c2, "a").await.unwrap();
    assert_eq!(a2.children[0].1, node_id(&cx, &c1, "a/b").await);
    // An empty change reuses the whole tree: same root node.
    let (c3, rows) = cx.commit(&[c2], &[&t2], &t2, 3).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(node_id(&cx, &c3, "").await, node_id(&cx, &c2, "").await);
}

#[tokio::test]
async fn deletions_remove_children_and_empty_directories() {
    let cx = MockCtx::new();
    let t1 = tree(&[("a/b/x", "x"), ("a/y", "y"), ("z", "z")]);
    let (c1, _) = cx.commit(&[], &[], &t1, 1).await;
    let t2 = tree(&[("a/y", "y"), ("z", "z")]);
    let (c2, _) = cx.commit(&[c1], &[&t1], &t2, 2).await;
    assert!(node(&cx, &c2, "a/b").await.is_none());
    assert!(node(&cx, &c2, "a/b/x").await.is_none());
    let (_, a) = node(&cx, &c2, "a").await.unwrap();
    assert_eq!(a.changeset, c2);
    assert_eq!(a.children.len(), 1);
    assert_eq!(a.children[0].0.as_bytes(), b"y");
    assert_eq!(a.children[0].1, node_id(&cx, &c1, "a/y").await);
    // Deleting everything leaves an empty root Dir.
    let (c3, _) = cx.commit(&[c2], &[&t2], &MTree::new(), 3).await;
    let (_, root) = node(&cx, &c3, "").await.unwrap();
    assert!(root.children.is_empty());
    assert_eq!(root.content, empty_tree());
    assert_eq!(root.parents, vec![node_id(&cx, &c2, "").await]);
}

#[tokio::test]
async fn file_directory_transitions_link_the_previous_node() {
    let cx = MockCtx::new();
    let t1 = tree(&[("a", "file a"), ("k", "k")]);
    let (c1, _) = cx.commit(&[], &[], &t1, 1).await;
    let t2 = tree(&[("a/b", "inner"), ("k", "k")]);
    let (c2, _) = cx.commit(&[c1], &[&t1], &t2, 2).await;
    let (_, a2) = node(&cx, &c2, "a").await.unwrap();
    assert_eq!(a2.kind, NodeKind::Dir);
    assert_eq!(a2.parents, vec![node_id(&cx, &c1, "a").await]);
    assert_eq!(a2.children.len(), 1);
    // And back: directory to file.
    let (c3, _) = cx.commit(&[c2], &[&t2], &t1, 3).await;
    let (_, a3) = node(&cx, &c3, "a").await.unwrap();
    assert_eq!(a3.kind, NodeKind::File);
    assert_eq!(a3.changeset, c3);
    assert_eq!(a3.parents, vec![node_id(&cx, &c2, "a").await]);
    assert!(node(&cx, &c3, "a/b").await.is_none());
}

#[tokio::test]
async fn mode_only_change_reuses_the_file_node() {
    let cx = MockCtx::new();
    let t1 = tree(&[("d/f", "same")]);
    let (c1, _) = cx.commit(&[], &[], &t1, 1).await;
    let mut t2 = t1.clone();
    t2.insert(
        b"d/f".to_vec(),
        MFile {
            mode: 2,
            content: b"same".to_vec(),
        },
    );
    let (c2, _) = cx.commit(&[c1], &[&t1], &t2, 2).await;
    // The node stores the blob id only: the file node is reused, its directory is new.
    assert_eq!(
        node_id(&cx, &c2, "d/f").await,
        node_id(&cx, &c1, "d/f").await
    );
    let (_, d) = node(&cx, &c2, "d").await.unwrap();
    assert_eq!(d.changeset, c2);
}

/// Base `B`, left `L` (changes `f`), right `R` (changes `g` and maybe `f`).
struct MergeFixture {
    cx: std::sync::Arc<MockCtx>,
    b: ObjectId,
    l: ObjectId,
    r: ObjectId,
    tl: MTree,
    tr: MTree,
}

async fn merge_fixture(right_f: &str) -> MergeFixture {
    let cx = MockCtx::new();
    let tb = tree(&[("f", "base f"), ("g", "base g"), ("s/h", "h")]);
    let (b, _) = cx.commit(&[], &[], &tb, 1).await;
    let tl = tree(&[("f", "left f"), ("g", "base g"), ("s/h", "h")]);
    let (l, _) = cx.commit(&[b], &[&tb], &tl, 2).await;
    let tr = tree(&[("f", right_f), ("g", "right g"), ("s/h", "h")]);
    let (r, _) = cx.commit(&[b], &[&tb], &tr, 3).await;
    MergeFixture {
        cx,
        b,
        l,
        r,
        tl,
        tr,
    }
}

#[tokio::test]
async fn merge_keeping_first_parent_version_reuses_its_node() {
    let fx = merge_fixture("base f").await;
    let cx = &fx.cx;
    // M = merge(L, R): f from L, g from R.
    let tm = tree(&[("f", "left f"), ("g", "right g"), ("s/h", "h")]);
    let (m, _) = cx.commit(&[fx.l, fx.r], &[&fx.tl, &fx.tr], &tm, 4).await;
    assert_eq!(node_id(cx, &m, "f").await, node_id(cx, &fx.l, "f").await);
    // g takes the second parent's version: a new node whose parents are both parents' nodes.
    let (_, g) = node(cx, &m, "g").await.unwrap();
    assert_eq!(g.changeset, m);
    assert_eq!(
        g.parents,
        vec![node_id(cx, &fx.l, "g").await, node_id(cx, &fx.r, "g").await]
    );
    // s/h is unchanged everywhere (one node since B).
    assert_eq!(
        node_id(cx, &m, "s/h").await,
        node_id(cx, &fx.b, "s/h").await
    );
    // The root node has both parents' roots as parents.
    let (_, root) = node(cx, &m, "").await.unwrap();
    assert_eq!(
        root.parents,
        vec![node_id(cx, &fx.l, "").await, node_id(cx, &fx.r, "").await]
    );
}

#[tokio::test]
async fn merge_taking_second_parent_version_creates_a_two_parent_node() {
    let fx = merge_fixture("right f").await;
    let cx = &fx.cx;
    let tm = tree(&[("f", "right f"), ("g", "base g"), ("s/h", "h")]);
    let (m, _) = cx.commit(&[fx.l, fx.r], &[&fx.tl, &fx.tr], &tm, 4).await;
    let (_, f) = node(cx, &m, "f").await.unwrap();
    assert_eq!(f.changeset, m);
    assert_eq!(
        f.parents,
        vec![node_id(cx, &fx.l, "f").await, node_id(cx, &fx.r, "f").await]
    );
    // g equals the first parent's version (base g): reused even though R changed it.
    assert_eq!(node_id(cx, &m, "g").await, node_id(cx, &fx.l, "g").await);
}

#[tokio::test]
async fn merge_parent_nodes_are_deduplicated_and_skip_absent_paths() {
    let fx = merge_fixture("base f").await;
    let cx = &fx.cx;
    // s/h has the same node in both parents; changing it in the merge gives one parent.
    // n exists only in the merge; q only in... nowhere: no parents.
    let tm = tree(&[
        ("f", "left f"),
        ("g", "right g"),
        ("s/h", "merged h"),
        ("n", "new"),
    ]);
    let (m, _) = cx.commit(&[fx.l, fx.r], &[&fx.tl, &fx.tr], &tm, 4).await;
    let (_, h) = node(cx, &m, "s/h").await.unwrap();
    assert_eq!(h.parents, vec![node_id(cx, &fx.b, "s/h").await]);
    let (_, n) = node(cx, &m, "n").await.unwrap();
    assert!(n.parents.is_empty());

    // A path present only in the second parent: its node's only parent is that parent's node.
    let tr2 = tree(&[
        ("f", "base f"),
        ("g", "base g"),
        ("s/h", "h"),
        ("only_r", "r"),
    ]);
    let (r2, _) = cx
        .commit(
            &[fx.b],
            &[&tree(&[("f", "base f"), ("g", "base g"), ("s/h", "h")])],
            &tr2,
            5,
        )
        .await;
    let tm2 = tree(&[
        ("f", "left f"),
        ("g", "base g"),
        ("s/h", "h"),
        ("only_r", "r2"),
    ]);
    let (m2, _) = cx.commit(&[fx.l, r2], &[&fx.tl, &tr2], &tm2, 6).await;
    let (_, o) = node(cx, &m2, "only_r").await.unwrap();
    assert_eq!(o.parents, vec![node_id(cx, &r2, "only_r").await]);
}

#[tokio::test]
async fn derivation_is_deterministic_and_idempotent() {
    let cx = MockCtx::new();
    let t1 = tree(&[("a/b", "1"), ("c", "2")]);
    let (c1, _) = cx.commit(&[], &[], &t1, 1).await;
    let t2 = tree(&[("a/b", "3"), ("c", "2"), ("d/e/f", "4")]);
    let cs = changeset(vec![c1], cx.put_tree(&t2), delta_of(&t1, &t2), 2);
    let id = scion_object::Canonical::id(&cs);
    let nt = new_trees(&t1, &t2);
    let first = cx.derive(&id, &cs, &nt, false).await.unwrap();
    let count = cx.node_count();
    let second = cx.derive(&id, &cs, &nt, false).await.unwrap();
    assert_eq!(first, second, "byte-identical rows");
    assert_eq!(cx.node_count(), count, "no new objects on re-derivation");
    // Identical content in a fresh context gives identical node ids.
    let cx2 = MockCtx::new();
    let (c1b, _) = cx2.commit(&[], &[], &t1, 1).await;
    assert_eq!(c1b, c1);
    let again = cx2.derive(&id, &cs, &nt, false).await.unwrap();
    assert_eq!(again, first);
}

#[tokio::test]
async fn one_file_change_writes_depth_plus_one_nodes() {
    let cx = MockCtx::new();
    let mut t1 = MTree::new();
    for i in 0..5 {
        for j in 0..5 {
            for k in 0..4 {
                t1.insert(
                    format!("d{i}/e{j}/g/f{k}").into_bytes(),
                    MFile {
                        mode: 1,
                        content: format!("{i}{j}{k}").into_bytes(),
                    },
                );
            }
        }
    }
    let (c1, _) = cx.commit(&[], &[], &t1, 1).await;
    let mut t2 = t1.clone();
    t2.insert(
        b"d3/e2/g/f1".to_vec(),
        MFile {
            mode: 1,
            content: b"changed".to_vec(),
        },
    );
    let puts = cx.puts.load(Ordering::Relaxed);
    let reads = cx.tree_reads.load(Ordering::Relaxed);
    let before = cx.node_count();
    let (c2, _) = cx.commit(&[c1], &[&t1], &t2, 2).await;
    // depth 4 (components) -> root, d3, d3/e2, d3/e2/g and the file: depth + 1 = 5.
    assert_eq!(cx.node_count() - before, 5);
    assert_eq!(cx.puts.load(Ordering::Relaxed) - puts, 5);
    assert_eq!(
        cx.tree_reads.load(Ordering::Relaxed),
        reads,
        "derivation reads no trees when every parent has a history row"
    );
    assert_eq!(node(&cx, &c2, "d3/e2/g/f1").await.unwrap().1.changeset, c2);
}

#[tokio::test]
async fn ancestors_without_a_history_row_are_backfilled() {
    // Reference: everything derived normally.
    let reference = MockCtx::new();
    let t0 = MTree::new();
    let t1 = tree(&[("a/x", "1"), ("b", "2")]);
    let t2 = tree(&[("a/x", "3"), ("b", "2"), ("c/d", "4")]);
    let t3 = tree(&[("a/x", "3"), ("c/d", "5")]);
    let (r0, _) = reference.commit(&[], &[], &t0, 0).await;
    let (r1, _) = reference.commit(&[r0], &[&t0], &t1, 1).await;
    let (r2, _) = reference.commit(&[r1], &[&t1], &t2, 2).await;
    let (r3, _) = reference.commit(&[r2], &[&t2], &t3, 3).await;

    // Same history, but c1 and c2 were stored without history rows.
    let cx = MockCtx::new();
    let (c0, _) = cx.commit(&[], &[], &t0, 0).await;
    let mut ids = vec![c0];
    for (seq, (a, b)) in [(&t0, &t1), (&t1, &t2)].into_iter().enumerate() {
        let parent = *ids.last().unwrap();
        let cs = changeset(vec![parent], cx.put_tree(b), delta_of(a, b), seq as u64 + 1);
        let id = scion_object::Canonical::id(&cs);
        cx.add_record(id, cs);
        ids.push(id);
    }
    assert_eq!(ids, vec![r0, r1, r2]);
    let (c3, rows) = cx.commit(&[ids[2]], &[&t2], &t3, 3).await;
    assert_eq!(c3, r3);
    let keys: Vec<&[u8]> = rows.iter().map(|r| &r.key.key[..]).collect();
    assert_eq!(keys.len(), 3, "own row plus two backfilled rows");
    assert_eq!(keys[0], c3.as_bytes());
    assert!(keys.contains(&r1.as_bytes().as_slice()));
    assert!(keys.contains(&r2.as_bytes().as_slice()));
    for cs in [r0, r1, r2, r3] {
        assert_eq!(
            history::root_node(cx.as_ref(), &cs).await.unwrap(),
            history::root_node(reference.as_ref(), &cs).await.unwrap()
        );
    }

    // root_node derives a missing value on demand (no row is written).
    let cx2 = MockCtx::new();
    let (d0, _) = cx2.commit(&[], &[], &t0, 0).await;
    let cs = changeset(vec![d0], cx2.put_tree(&t1), delta_of(&t0, &t1), 1);
    let d1 = scion_object::Canonical::id(&cs);
    cx2.add_record(d1, cs);
    assert_eq!(
        history::root_node(cx2.as_ref(), &d1).await.unwrap(),
        history::root_node(reference.as_ref(), &r1).await.unwrap()
    );
    assert!(cx2.stored.lock().get(&history::derived_key(&d1)).is_none());
}

#[tokio::test]
async fn lookups_report_errors() {
    let cx = MockCtx::new();
    let unknown = ObjectId::new(Kind::Changeset, [9; 32]);
    let e = history::root_node(cx.as_ref(), &unknown).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    let e = history::node_for_path(cx.as_ref(), &unknown, &p("a"))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    let e = history::get_node(cx.as_ref(), &empty_tree())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert_eq!(e.subcode(), Some(subcode::INVALID_KIND));
    let missing = ObjectId::new(Kind::Derived(0x10), [1; 32]);
    let e = history::get_node(cx.as_ref(), &missing).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn inconsistent_eager_input_is_rejected() {
    let cx = MockCtx::new();
    let t0 = MTree::new();
    let (c0, _) = cx.commit(&[], &[], &t0, 0).await;
    let t1 = tree(&[("a/b", "x")]);
    let root = cx.put_tree(&t1);
    let cs: Changeset = changeset(vec![c0], root, delta_of(&t0, &t1), 1);
    let id = scion_object::Canonical::id(&cs);
    // new_trees lacks the directory "a" that the Put needs.
    let e = cx.derive(&id, &cs, &[], false).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    // A directory entry at a path the delta puts as a file.
    let mut bad = new_trees(&t0, &t1);
    bad.push((p("a/b"), bad[0].1));
    let e = cx.derive(&id, &cs, &bad, false).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    // A Put whose parent directory is deleted.
    let cs2 = Changeset {
        delta: vec![
            DeltaEntry {
                path: p("a"),
                op: DeltaOp::Delete,
            },
            cs.delta[0].clone(),
        ],
        ..cs.clone()
    };
    let e = cx.derive(&id, &cs2, &[], false).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
}
