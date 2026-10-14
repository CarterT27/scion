//! SPEC 9.2 / 19.6 history nodes end to end: `History` with `DerivedEngine`
//! (`Registry::v1_default`) as the EagerHook. Covers the bootstrap root, the node reuse rule on
//! merges, and "eager values exist for every visible changeset" under fault injection between
//! derivation and insert (Crash at `create.after_derive` and `meta.commit_changeset.before`,
//! fail-after-commit at `meta.commit_changeset.after`), each checked with fsck using the
//! engine's required eager types.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod history_support;

use history_support::*;
use scion_core::{ErrorCode, FaultAction, ObjectId, fault::points};
use scion_derived::history;
use scion_object::{Extensions, NodeKind};
use scion_refmodel::MTree;
use scion_store::Violation;

async fn node_id(s: &Stack, cs: &ObjectId, path: &str) -> ObjectId {
    history::node_for_path(s.engine.ctx(), cs, &p(path))
        .await
        .unwrap()
        .expect("path exists")
        .0
}

#[tokio::test]
async fn bootstrap_root_has_a_childless_root_node_row() {
    let s = stack();
    let root = s.h.bootstrap().await.unwrap();
    let stored = s
        .engine
        .get_stored(history::NAME, None, root.as_bytes())
        .await
        .unwrap()
        .expect("history row of the root changeset");
    let node_id = ObjectId::from_bytes(&stored).unwrap();
    let n = history::get_node(s.engine.ctx(), &node_id).await.unwrap();
    assert_eq!(n.kind, NodeKind::Dir);
    assert_eq!(n.content, empty_tree());
    assert_eq!(n.changeset, root);
    assert!(n.children.is_empty() && n.parents.is_empty());
    assert!(s.fsck().await.is_empty());
}

#[tokio::test]
async fn merges_follow_the_node_reuse_rule() {
    let s = stack();
    let root = s.h.bootstrap().await.unwrap();
    let t0 = MTree::new();
    let tb = tree(&[
        ("src/f", "base f\n"),
        ("src/g", "base g\n"),
        ("doc/h", "h\n"),
    ]);
    let b = s
        .commit_tree(vec![root], &t0, &tb, 1, false, Extensions::new())
        .await
        .unwrap();
    let tl = tree(&[
        ("src/f", "left f\n"),
        ("src/g", "base g\n"),
        ("doc/h", "h\n"),
    ]);
    let l = s
        .commit_tree(vec![b], &tb, &tl, 2, false, Extensions::new())
        .await
        .unwrap();
    let tr = tree(&[
        ("src/f", "right f\n"),
        ("src/g", "right g\n"),
        ("doc/h", "h\n"),
    ]);
    let r = s
        .commit_tree(vec![b], &tb, &tr, 3, false, Extensions::new())
        .await
        .unwrap();

    // M1 keeps the first parent's f and takes the second parent's g.
    let tm1 = tree(&[
        ("src/f", "left f\n"),
        ("src/g", "right g\n"),
        ("doc/h", "h\n"),
    ]);
    let m1 = s
        .commit_tree(vec![l, r], &tl, &tm1, 4, false, Extensions::new())
        .await
        .unwrap();
    assert_eq!(
        node_id(&s, &m1, "src/f").await,
        node_id(&s, &l, "src/f").await
    );
    let (_, g) = history::node_for_path(s.engine.ctx(), &m1, &p("src/g"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(g.changeset, m1);
    assert_eq!(
        g.parents,
        vec![
            node_id(&s, &l, "src/g").await,
            node_id(&s, &r, "src/g").await
        ]
    );
    assert_eq!(node_id(&s, &m1, "doc").await, node_id(&s, &b, "doc").await);

    // M2 takes the second parent's f: a new node with both parents' nodes as parents.
    let tm2 = tree(&[
        ("src/f", "right f\n"),
        ("src/g", "base g\n"),
        ("doc/h", "h\n"),
    ]);
    let m2 = s
        .commit_tree(vec![l, r], &tl, &tm2, 5, false, Extensions::new())
        .await
        .unwrap();
    let (_, f) = history::node_for_path(s.engine.ctx(), &m2, &p("src/f"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.changeset, m2);
    assert_eq!(
        f.parents,
        vec![
            node_id(&s, &l, "src/f").await,
            node_id(&s, &r, "src/f").await
        ]
    );
    // g equals the first parent's version: reused although R changed it.
    assert_eq!(
        node_id(&s, &m2, "src/g").await,
        node_id(&s, &l, "src/g").await
    );
    assert!(s.fsck().await.is_empty());
}

/// Two changesets on top of the root, then a third attempted with `point` armed as `action`.
async fn faulted_create(point: &str, action: FaultAction) -> (Stack, ObjectId, MTree, MTree) {
    let s = stack();
    let root = s.h.bootstrap().await.unwrap();
    let t0 = MTree::new();
    let t1 = tree(&[("a/b/c", "1\n"), ("a/d", "2\n")]);
    let c1 = s
        .commit_tree(vec![root], &t0, &t1, 1, false, Extensions::new())
        .await
        .unwrap();
    let t2 = tree(&[("a/b/c", "3\n"), ("a/d", "2\n"), ("e", "4\n")]);
    let faults = s.h.objects().storage().faults.clone();
    faults.arm(point, action, Some(1));
    let r = s
        .commit_tree(vec![c1], &t1, &t2, 2, false, Extensions::new())
        .await;
    let e = r.expect_err("the armed fault fails the create");
    assert_eq!(e.code, ErrorCode::Unavailable);
    faults.reset();
    faults.disarm_all();
    (s, c1, t1, t2)
}

fn assert_no_missing_eager(v: &[Violation]) {
    assert!(v.is_empty(), "fsck violations: {v:?}");
}

#[tokio::test]
async fn crash_between_derive_and_insert_leaves_no_visible_changeset_without_eager_values() {
    for point in [points::CREATE_AFTER_DERIVE, "meta.commit_changeset.before"] {
        let (s, c1, t1, t2) = faulted_create(point, FaultAction::Crash).await;
        let changesets = s.mem.meta.all_changesets().len();
        // Bootstrap root + c1 only: the crashed changeset is not visible.
        assert_eq!(changesets, 2, "{point}");
        assert_no_missing_eager(&s.fsck().await);
        // The client retry succeeds and derives the eager values.
        let c2 = s
            .commit_tree(vec![c1], &t1, &t2, 2, false, Extensions::new())
            .await
            .unwrap();
        assert_no_missing_eager(&s.fsck().await);
        let (_, n) = history::node_for_path(s.engine.ctx(), &c2, &p("a/b/c"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n.changeset, c2);
        assert_eq!(n.parents, vec![node_id(&s, &c1, "a/b/c").await]);
    }
}

#[tokio::test]
async fn fail_after_commit_still_has_eager_values() {
    let (s, c1, t1, t2) =
        faulted_create("meta.commit_changeset.after", FaultAction::FailAfter).await;
    // The row was committed (response lost) together with its eager values.
    assert_eq!(s.mem.meta.all_changesets().len(), 3);
    assert_no_missing_eager(&s.fsck().await);
    let c2 = s
        .commit_tree(vec![c1], &t1, &t2, 2, false, Extensions::new())
        .await
        .unwrap();
    assert!(
        s.engine
            .get_stored(history::NAME, Some(history::VERSION), c2.as_bytes())
            .await
            .unwrap()
            .is_some()
    );
    assert_no_missing_eager(&s.fsck().await);
}

#[tokio::test]
async fn fsck_reports_a_missing_history_row() {
    // The oracle detects the violation these tests guard against: a changeset row committed
    // without its history value.
    let s = stack();
    s.h.bootstrap().await.unwrap();
    let cs = changeset(Vec::new(), empty_tree(), Vec::new(), 77);
    let encoded = bytes::Bytes::from(scion_object::Canonical::encode(&cs));
    let id = scion_object::Canonical::id(&cs);
    let meta = sample_meta(Extensions::new());
    s.h.objects()
        .put_encoded(
            scion_core::Kind::Meta,
            bytes::Bytes::from(scion_object::Canonical::encode(&meta)),
        )
        .await
        .unwrap();
    s.h.meta()
        .commit_changeset(scion_store::ChangesetBundle {
            row: scion_store::ChangesetRow {
                id,
                encoded,
                generation: 1,
                skip: Vec::new(),
                change_id: cs.change_id,
                created_by: "service:test".to_owned(),
                created_at: std::time::SystemTime::UNIX_EPOCH,
            },
            parents: Vec::new(),
            object_paths: Vec::new(),
            derived: Vec::new(),
        })
        .await
        .unwrap();
    let v = s.fsck().await;
    assert!(
        v.iter().any(|x| matches!(
            x,
            Violation::MissingEager { changeset, ty } if *changeset == id && ty == history::NAME
        )),
        "{v:?}"
    );
}

/// The node set of `path` at `cs` walked through node parents, as changeset indices of `ids`,
/// compared with the reference model built from the same trees.
async fn assert_matches_model(s: &Stack, ids: &[ObjectId], trees: &[(Vec<usize>, MTree)]) {
    let mut m = scion_refmodel::MRepo::new();
    for (parents, t) in trees {
        m.commit(parents, t.clone());
    }
    let mut cache = NodeCache::default();
    for (c, (_, t)) in trees.iter().enumerate() {
        let mut paths = all_paths(t);
        for (_, other) in trees {
            paths.extend(all_paths(other));
        }
        for path in paths {
            let rp = scion_object::RepoPath::parse(&path).unwrap();
            let node = history::node_for_path(s.engine.ctx(), &ids[c], &rp)
                .await
                .unwrap();
            for fp in [false, true] {
                let want: std::collections::BTreeSet<usize> =
                    m.log_path(c, &path, fp).into_iter().collect();
                let got: std::collections::BTreeSet<usize> = match &node {
                    Some((id, _)) => cache
                        .reachable_changesets(s.engine.ctx(), *id, fp)
                        .await
                        .into_iter()
                        .map(|cs| ids.iter().position(|x| *x == cs).unwrap())
                        .collect(),
                    None => Default::default(),
                };
                assert_eq!(
                    got,
                    want,
                    "commit {c}, path {:?}, first_parent_only {fp}",
                    String::from_utf8_lossy(&path)
                );
            }
        }
    }
}

#[tokio::test]
async fn emptying_the_tree_and_re_adding_a_path() {
    // c2 deletes every file: its root tree is the empty tree, which History passes in
    // `new_trees` at the root path. c3 re-adds c1's content: the path is absent in c3's only
    // parent, so the new node has no parents (the deleted interval breaks the chain).
    let s = stack();
    let root = s.h.bootstrap().await.unwrap();
    let t0 = MTree::new();
    let t1 = tree(&[("a/b/f", "one\n"), ("g", "two\n")]);
    let c1 = s
        .commit_tree(vec![root], &t0, &t1, 1, false, Extensions::new())
        .await
        .unwrap();
    let c2 = s
        .commit_tree(vec![c1], &t1, &t0, 2, false, Extensions::new())
        .await
        .unwrap();
    let r2 = history::root_node(s.engine.ctx(), &c2).await.unwrap();
    let n2 = history::get_node(s.engine.ctx(), &r2).await.unwrap();
    assert_eq!(n2.kind, NodeKind::Dir);
    assert_eq!(n2.content, empty_tree());
    assert_eq!(n2.changeset, c2);
    assert!(n2.children.is_empty());
    assert_eq!(n2.parents, vec![node_id(&s, &c1, "").await]);
    for gone in ["a", "a/b", "a/b/f", "g"] {
        assert!(
            history::node_for_path(s.engine.ctx(), &c2, &p(gone))
                .await
                .unwrap()
                .is_none(),
            "{gone}"
        );
    }
    let t3 = tree(&[("a/b/f", "one\n")]);
    let c3 = s
        .commit_tree(vec![c2], &t0, &t3, 3, false, Extensions::new())
        .await
        .unwrap();
    let (_, f3) = history::node_for_path(s.engine.ctx(), &c3, &p("a/b/f"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f3.changeset, c3);
    assert!(f3.parents.is_empty());
    assert_ne!(
        node_id(&s, &c3, "a/b/f").await,
        node_id(&s, &c1, "a/b/f").await
    );
    // Same content as c1's tree at "a": a different node (created by c3), not c1's node.
    let (_, a3) = history::node_for_path(s.engine.ctx(), &c3, &p("a"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a3.changeset, c3);
    assert_matches_model(
        &s,
        &[root, c1, c2, c3],
        &[
            (vec![], t0.clone()),
            (vec![0], t1.clone()),
            (vec![1], t0.clone()),
            (vec![2], t3.clone()),
        ],
    )
    .await;
    assert!(s.fsck().await.is_empty());
}

#[tokio::test]
async fn reverting_to_an_older_version_creates_a_new_node() {
    // Reuse compares with parents[0] only: restoring a grandparent's content is a change.
    let s = stack();
    let root = s.h.bootstrap().await.unwrap();
    let t0 = MTree::new();
    let t1 = tree(&[("d/f", "v1\n"), ("d/g", "same\n")]);
    let t2 = tree(&[("d/f", "v2\n"), ("d/g", "same\n")]);
    let c1 = s
        .commit_tree(vec![root], &t0, &t1, 1, false, Extensions::new())
        .await
        .unwrap();
    let c2 = s
        .commit_tree(vec![c1], &t1, &t2, 2, false, Extensions::new())
        .await
        .unwrap();
    let c3 = s
        .commit_tree(vec![c2], &t2, &t1, 3, false, Extensions::new())
        .await
        .unwrap();
    for path in ["", "d", "d/f"] {
        let (_, n) = history::node_for_path(s.engine.ctx(), &c3, &p(path))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n.changeset, c3, "{path}");
        assert_eq!(n.parents, vec![node_id(&s, &c2, path).await], "{path}");
        assert_ne!(node_id(&s, &c3, path).await, node_id(&s, &c1, path).await);
    }
    // Untouched sibling: c1's node all along.
    assert_eq!(node_id(&s, &c3, "d/g").await, node_id(&s, &c1, "d/g").await);
    assert_matches_model(
        &s,
        &[root, c1, c2, c3],
        &[
            (vec![], t0.clone()),
            (vec![0], t1.clone()),
            (vec![1], t2.clone()),
            (vec![2], t1.clone()),
        ],
    )
    .await;
    assert!(s.fsck().await.is_empty());
}

#[tokio::test]
async fn octopus_merge_adding_paths_from_later_parents() {
    // A 3-parent merge: a path only in the third parent gets a node whose only parent is the
    // third parent's node; a path equal in all parents is the first parent's node.
    let s = stack();
    let root = s.h.bootstrap().await.unwrap();
    let t0 = MTree::new();
    let tb = tree(&[("common", "c\n")]);
    let b = s
        .commit_tree(vec![root], &t0, &tb, 1, false, Extensions::new())
        .await
        .unwrap();
    let tx = tree(&[("common", "c\n"), ("x", "x\n")]);
    let ty = tree(&[("common", "c\n"), ("y", "y\n")]);
    let x = s
        .commit_tree(vec![b], &tb, &tx, 2, false, Extensions::new())
        .await
        .unwrap();
    let y = s
        .commit_tree(vec![b], &tb, &ty, 3, false, Extensions::new())
        .await
        .unwrap();
    let tm = tree(&[("common", "c\n"), ("x", "x\n"), ("y", "y\n")]);
    let m = s
        .commit_tree(vec![b, x, y], &tb, &tm, 4, false, Extensions::new())
        .await
        .unwrap();
    assert_eq!(
        node_id(&s, &m, "common").await,
        node_id(&s, &b, "common").await
    );
    let (_, ny) = history::node_for_path(s.engine.ctx(), &m, &p("y"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ny.changeset, m);
    assert_eq!(ny.parents, vec![node_id(&s, &y, "y").await]);
    let (_, nr) = history::node_for_path(s.engine.ctx(), &m, &p(""))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        nr.parents,
        vec![
            node_id(&s, &b, "").await,
            node_id(&s, &x, "").await,
            node_id(&s, &y, "").await
        ]
    );
    assert_matches_model(
        &s,
        &[root, b, x, y, m],
        &[
            (vec![], t0.clone()),
            (vec![0], tb.clone()),
            (vec![1], tx.clone()),
            (vec![1], ty.clone()),
            (vec![1, 2, 3], tm.clone()),
        ],
    )
    .await;
    assert!(s.fsck().await.is_empty());
}
