//! SPEC 19.4 rebase properties: determinism (across runs, engine instances and recreated
//! storage handles, independent of clock and caller), the `X == P` fast path, EMPTY_CHANGE,
//! multi_parent, result correctness, and the access-counter checks (zero blob reads when `X`
//! changed only unrelated paths; O(|delta| x depth) tree reads).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod merge_support;

use merge_support::*;
use scion_core::{ErrorCode, FaultRegistry, ObjectId, subcode};
use scion_history::{HistoryConfig, Rebaser};
use scion_store::Storage;
use std::time::Duration;

/// `P`, `X = P + unrelated change`, `C = P + change`.
async fn setup(env: &Env) -> (ObjectId, ObjectId, ObjectId) {
    let h = &env.h;
    let p = commit(
        h,
        env.root,
        &[
            ("src/lib/a.txt", file("a\n")),
            ("src/lib/b.txt", file("b\n")),
            ("docs/readme", file("readme\n")),
        ],
    )
    .await;
    let x = commit(h, p, &[("docs/readme", file("readme v2\n"))]).await;
    let c = commit(
        h,
        p,
        &[
            ("src/lib/a.txt", file("a v2\n")),
            ("src/new.txt", file("new\n")),
        ],
    )
    .await;
    (p, x, c)
}

#[tokio::test]
async fn same_inputs_same_id_everywhere() {
    let env = env().await;
    let (_, x, c) = setup(&env).await;
    let first = env.h.rebase(&c, &x, &alice()).await.unwrap();
    // Again (idempotent), by another caller, later on the clock.
    env.clock.advance(Duration::from_secs(3600));
    let again = env.h.rebase(&c, &x, &bob()).await.unwrap();
    assert_eq!(first, again);
    // Through the Rebaser seam.
    let seam = Rebaser::rebase(&env.h, &c, &x, &alice()).await.unwrap();
    assert_eq!(first, seam);

    // A second engine instance over the same storage (fresh caches, different clock).
    let clock2 = scion_core::ManualClock::new(start_time() + Duration::from_secs(99_999));
    let h2 = history_over(
        env.storage.clone(),
        HistoryConfig::default(),
        clock2.clone(),
    );
    assert_eq!(h2.rebase(&c, &x, &bob()).await.unwrap(), first);

    // Recreated storage handles over the same backends ("after restart").
    let storage3 = Storage::new(
        env.mem.meta.clone(),
        env.mem.blobs.clone(),
        FaultRegistry::new(),
    );
    let h3 = history_over(storage3, HistoryConfig::default(), clock2);
    assert_eq!(h3.rebase(&c, &x, &alice()).await.unwrap(), first);

    // A completely separate repository built from the same inputs gives the same id.
    let other = merge_support::env().await;
    let (_, x2, c2) = setup(&other).await;
    assert_eq!((x2, c2), (x, c));
    other.clock.advance(Duration::from_secs(12_345));
    assert_eq!(other.h.rebase(&c2, &x2, &bob()).await.unwrap(), first);

    // The result keeps C's meta and change id, has parents [X] and the recomputed delta.
    let src = env.h.changeset(&c).await.unwrap().changeset;
    let res = env.h.changeset(&first).await.unwrap().changeset;
    assert_eq!(res.parents, vec![x]);
    assert_eq!(res.meta, src.meta);
    assert_eq!(res.change_id, src.change_id);
    assert_eq!(res.delta, src.delta);
    let tree = snapshot(&env.h, &res.root_tree).await;
    let keys: Vec<&[u8]> = tree.keys().map(|k| k.as_slice()).collect();
    assert_eq!(
        keys,
        vec![
            b"docs/readme".as_slice(),
            b"src/lib/a.txt",
            b"src/lib/b.txt",
            b"src/new.txt"
        ]
    );
    assert_eq!(tree[b"docs/readme".as_slice()].content, b"readme v2\n");
    assert_eq!(tree[b"src/lib/a.txt".as_slice()].content, b"a v2\n");
    assert_fsck(&env).await;
}

async fn assert_fsck(env: &Env) {
    let opts = scion_store::FsckOptions {
        required_eager: vec![("commit_graph".to_owned(), 1), ("path_index".to_owned(), 1)],
        verify_bytes: true,
    };
    let v = scion_store::fsck(env.h.objects(), &opts).await.unwrap();
    assert!(v.is_empty(), "fsck: {v:?}");
}

#[tokio::test]
async fn fast_path_returns_c() {
    let env = env().await;
    let (p, _, c) = setup(&env).await;
    let before = env.storage.counters.snapshot();
    assert_eq!(env.h.rebase(&c, &p, &alice()).await.unwrap(), c);
    let d = env.storage.counters.snapshot() - before;
    assert_eq!(d.trees_read, 0);
    assert_eq!(d.objects_written, 0);
}

#[tokio::test]
async fn empty_change_when_already_in_onto() {
    let env = env().await;
    let (p, x, c) = setup(&env).await;
    // X' already contains C's change (and more).
    let x2 = commit(
        &env.h,
        x,
        &[
            ("src/lib/a.txt", file("a v2\n")),
            ("src/new.txt", file("new\n")),
            ("zzz", file("z\n")),
        ],
    )
    .await;
    let err = env.h.rebase(&c, &x2, &alice()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::EmptyChange);
    // A rebased result rebased onto itself's tip is also empty.
    let r = env.h.rebase(&c, &x, &alice()).await.unwrap();
    let err = env.h.rebase(&c, &r, &alice()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::EmptyChange);
    let _ = p;
}

#[tokio::test]
async fn multi_parent_is_rejected() {
    let env = env().await;
    let (_, x, c) = setup(&env).await;
    let m = env
        .h
        .merge(
            &x,
            &c,
            fields("merge"),
            change_id_for(&[x, c], "merge"),
            &alice(),
            &scion_history::AllowAll,
        )
        .await
        .unwrap();
    let err = env.h.rebase(&m, &env.root, &alice()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert_eq!(err.subcode(), Some(subcode::MULTI_PARENT));
    // The parentless root changeset cannot be rebased either.
    let err = env.h.rebase(&env.root, &x, &alice()).await.unwrap_err();
    assert_eq!(err.subcode(), Some(subcode::MULTI_PARENT));
}

/// A deep path of `depth` components under `top`.
fn deep(top: &str, depth: usize, leaf: &str) -> String {
    let mut s = top.to_owned();
    for i in 1..depth - 1 {
        s.push_str(&format!("/d{i}"));
    }
    s.push('/');
    s.push_str(leaf);
    s
}

#[tokio::test]
async fn unrelated_onto_reads_no_blobs_and_bounded_trees() {
    for depth in [3usize, 6, 9] {
        let env = env().await;
        let h = &env.h;
        let a = deep("a", depth, "f");
        let z = deep("z", depth, "g");
        let mut base: Vec<(String, String)> = Vec::new();
        for i in 0..20 {
            base.push((deep("a", depth, &format!("s{i}")), format!("{i}\n")));
            base.push((deep("z", depth, &format!("s{i}")), format!("{i}\n")));
            base.push((format!("m{i}/x"), format!("{i}\n")));
        }
        base.push((a.clone(), "f\n".to_owned()));
        base.push((z.clone(), "g\n".to_owned()));
        let changes: Vec<Change<'_>> = base
            .iter()
            .map(|(p, c)| (p.as_str(), file(c.as_str())))
            .collect();
        let p = commit(h, env.root, &changes).await;
        let x = commit(h, p, &[(z.as_str(), file("g2\n"))]).await;
        let c = commit(h, p, &[(a.as_str(), file("f2\n"))]).await;

        let before = env.storage.counters.snapshot();
        let r = h.rebase(&c, &x, &alice()).await.unwrap();
        let d = env.storage.counters.snapshot() - before;
        let depth = depth as u64;
        eprintln!(
            "depth {depth}: trees_read {}, blobs_read {}, trees_written {}",
            d.trees_read, d.blobs_read, d.trees_written
        );
        assert_eq!(d.blobs_read, 0, "no blob is read for an unrelated onto");
        // B and O walks (shared directories read once) plus apply_delta over O.
        assert!(
            d.trees_read <= 3 * depth + 1,
            "depth {depth}: {} trees read",
            d.trees_read
        );
        assert!(d.trees_written <= depth);
        assert_eq!(content_at(h, &r, &a).await.unwrap(), b"f2\n");
        assert_eq!(content_at(h, &r, &z).await.unwrap(), b"g2\n");
    }
}

#[tokio::test]
async fn tree_reads_scale_with_delta_not_repository() {
    // Same delta, repositories of different size: identical counters.
    let mut seen = Vec::new();
    for width in [5usize, 200] {
        let env = env().await;
        let h = &env.h;
        let mut base: Vec<(String, String)> = Vec::new();
        for i in 0..width {
            base.push((format!("a/b/c/s{i}"), format!("{i}\n")));
            base.push((format!("w{i}/x"), format!("{i}\n")));
        }
        base.push(("a/b/c/f".to_owned(), "f\n".to_owned()));
        let changes: Vec<Change<'_>> = base
            .iter()
            .map(|(p, c)| (p.as_str(), file(c.as_str())))
            .collect();
        let p = commit(h, env.root, &changes).await;
        let x = commit(h, p, &[("w0/x", file("changed\n"))]).await;
        let c = commit(h, p, &[("a/b/c/f", file("f2\n")), ("a/n", file("n\n"))]).await;
        let before = env.storage.counters.snapshot();
        h.rebase(&c, &x, &alice()).await.unwrap();
        let d = env.storage.counters.snapshot() - before;
        assert_eq!(d.blobs_read, 0);
        seen.push((d.trees_read, d.trees_written, d.blobs_read));
    }
    assert_eq!(seen[0], seen[1]);
}

#[tokio::test]
async fn rebase_chain_is_deterministic() {
    // Rebasing a stack member by member (as Land does) twice gives the same ids.
    let env = env().await;
    let h = &env.h;
    let p = commit(h, env.root, &[("f", file("1\n2\n3\n4\n5\n"))]).await;
    let x = commit(h, p, &[("f", file("one\n2\n3\n4\n5\n"))]).await;
    let c1 = commit(h, p, &[("f", file("1\n2\n3\n4\nfive\n"))]).await;
    let c2 = commit(h, c1, &[("g", file("g\n"))]).await;
    let run = || async {
        let r1 = h.rebase(&c1, &x, &alice()).await.unwrap();
        let r2 = h.rebase(&c2, &r1, &alice()).await.unwrap();
        (r1, r2)
    };
    let a = run().await;
    let b = run().await;
    assert_eq!(a, b);
    assert_eq!(
        content_at(h, &a.1, "f").await.unwrap(),
        b"one\n2\n3\n4\nfive\n"
    );
    assert_no_markers(&env).await;
}
