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
