//! SPEC 8.3 `Merge(a, b)`: table rows through the merge form (row 2 `t == b` in particular,
//! which a rebase delta cannot exercise), structural rules for paths `b` changed, redacted
//! content (19.13), write authorization against `a`, unrelated roots (empty base, PLAN 6.14),
//! criss-cross histories (AMBIGUOUS_MERGE_BASE, Appendix A), the result shape, and the marker
//! scan (19.4).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod merge_support;

use merge_support::*;
use scion_core::{ConflictKind, ErrorCode, ObjectId, empty_tree_id};
use scion_history::{AllowAll, WriteAuthorizer};
use scion_object::RepoPath;

/// `P`, `a = P + ours`, `b = P + theirs`.
async fn three_way(
    env: &Env,
    base: &[Change<'_>],
    ours: &[Change<'_>],
    theirs: &[Change<'_>],
) -> (ObjectId, ObjectId, ObjectId) {
    let p = commit(&env.h, env.root, base).await;
    let a = commit(&env.h, p, ours).await;
    let b = commit(&env.h, p, theirs).await;
    (p, a, b)
}

async fn do_merge(env: &Env, a: &ObjectId, b: &ObjectId) -> scion_core::Result<ObjectId> {
    env.h
        .merge(
            a,
            b,
            fields("merge"),
            change_id_for(&[*a, *b], "merge"),
            &bob(),
            &AllowAll,
        )
        .await
}

#[tokio::test]
async fn row2_theirs_equals_base_takes_ours_and_result_shape() {
    let env = env().await;
    let (p, a, b) = three_way(
        &env,
        &[("f", file("f\n")), ("g", file("g\n"))],
        &[("f", file("ours\n"))],
        &[("g", file("theirs\n"))],
    )
    .await;
    let m = do_merge(&env, &a, &b).await.unwrap();
    let rec = env.h.changeset(&m).await.unwrap();
    assert_eq!(rec.changeset.parents, vec![a, b]);
    // f: t == b, take ours (not in the delta against a); g: o == b, take theirs.
    let paths: Vec<RepoPath> = rec.changeset.delta.iter().map(|e| e.path.clone()).collect();
    assert_eq!(paths, vec![p_("g")]);
    assert_eq!(content_at(&env.h, &m, "f").await.unwrap(), b"ours\n");
    assert_eq!(content_at(&env.h, &m, "g").await.unwrap(), b"theirs\n");
    // New Meta from the fields, creator = the caller.
    let meta = env.h.objects().get_meta(&rec.changeset.meta).await.unwrap();
    assert_eq!(meta.message, b"merge");
    assert_eq!(meta.creator, bob().id_string().into_bytes());
    assert_eq!(rec.changeset.change_id, change_id_for(&[a, b], "merge"));
    // Idempotent.
    assert_eq!(do_merge(&env, &a, &b).await.unwrap(), m);
    // The merge is a descendant of both sides and of the base.
    assert!(env.h.is_ancestor(&p, &m).await.unwrap());
    assert!(env.h.is_ancestor(&b, &m).await.unwrap());
}

fn p_(s: &str) -> RepoPath {
    p(s)
}

#[tokio::test]
async fn rows_and_conflicts_through_merge() {
    let env = env().await;
    let (_, a, b) = three_way(
        &env,
        &[
            ("same", file("s\n")),
            ("txt", file("1\n2\n3\n4\n5\n")),
            ("cc", file("1\n2\n3\n")),
            ("md", file("m\n")),
            ("ln", link("t0")),
            ("bin", raw(scion_core::Mode::File, b"\0base")),
        ],
        &[
            ("same", file("both\n")),
            ("txt", file("one\n2\n3\n4\n5\n")),
            ("cc", file("1\nours\n3\n")),
            ("md", DEL),
            ("ln", link("t1")),
            ("bin", raw(scion_core::Mode::File, b"\0ours")),
            ("aa", file("a-ours\n")),
        ],
        &[
            ("same", file("both\n")),
            ("txt", file("1\n2\n3\n4\nfive\n")),
            ("cc", file("1\ntheirs\n3\n")),
            ("md", file("m2\n")),
            ("ln", link("t2")),
            ("bin", raw(scion_core::Mode::File, b"\0theirs")),
            ("aa", file("a-theirs\n")),
        ],
    )
    .await;
    let err = do_merge(&env, &a, &b).await.unwrap_err();
    let r = report(&err);
    assert_eq!(
        kinds(r),
        vec![
            ("aa".to_owned(), ConflictKind::AddAdd),
            ("bin".to_owned(), ConflictKind::Binary),
            ("cc".to_owned(), ConflictKind::Content),
            ("ln".to_owned(), ConflictKind::Symlink),
            ("md".to_owned(), ConflictKind::ModifyDelete),
        ]
    );
    let cc = &r.conflicts[2];
    assert!(!cc.hunks.is_empty());
    assert_eq!(cc.ours.unwrap().id, ObjectId::hash_blob(b"1\nours\n3\n"));
    let clean: Vec<String> = r
        .clean_entries
        .iter()
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();
    assert_eq!(clean, vec!["same".to_owned(), "txt".to_owned()]);
    let txt = r.clean_entries[1].entry.unwrap();
    assert_eq!(txt.id, ObjectId::hash_blob(b"one\n2\n3\n4\nfive\n"));
    assert_no_markers(&env).await;
}

#[tokio::test]
async fn structural_rules_through_merge() {
    let env = env().await;
    // b puts a file where a has a directory; b puts a file below a's file; b deletes a file
    // where a has a directory.
    let (_, a, b) = three_way(
        &env,
        &[("keep", file("k\n")), ("del", file("d\n"))],
        &[
            ("dir/x", file("x\n")),
            ("f", file("file\n")),
            ("del", DEL),
            ("del/y", file("y\n")),
        ],
        &[("dir", file("file\n")), ("f/z", file("z\n")), ("del", DEL)],
    )
    .await;
    let err = do_merge(&env, &a, &b).await.unwrap_err();
    assert_eq!(
        kinds(report(&err)),
        vec![
            ("del".to_owned(), ConflictKind::FileDirectory),
            ("dir".to_owned(), ConflictKind::FileDirectory),
            ("f/z".to_owned(), ConflictKind::FileDirectory),
        ]
    );
    // A transition made only by ours (theirs untouched there) is not a conflict.
    let env = merge_support::env().await;
    let (_, a, b) = three_way(
        &env,
        &[("d", file("file\n")), ("g", file("g\n"))],
        &[("d", DEL), ("d/x", file("x\n"))],
        &[("g", file("g2\n"))],
    )
    .await;
    let m = do_merge(&env, &a, &b).await.unwrap();
    assert_eq!(content_at(&env.h, &m, "d/x").await.unwrap(), b"x\n");
    assert_eq!(content_at(&env.h, &m, "g").await.unwrap(), b"g2\n");
}

#[tokio::test]
async fn merge_needing_redacted_content_is_a_redacted_conflict() {
    // SPEC 19.13 / 13.6.
    let env = env().await;
    let (_, a, b) = three_way(
        &env,
        &[("f", file("1\n2\n3\n")), ("g", file("g\n"))],
        &[("f", file("one\n2\n3\n"))],
        &[("f", file("1\n2\nthree\n")), ("g", file("secret\n"))],
    )
    .await;
    redact(&env, &[ObjectId::hash_blob(b"1\n2\nthree\n")]).await;
    redact(&env, &[ObjectId::hash_blob(b"secret\n")]).await;
    let err = do_merge(&env, &a, &b).await.unwrap_err();
    let r = report(&err);
    assert_eq!(kinds(r), vec![("f".to_owned(), ConflictKind::Redacted)]);
    // g's redacted content is not needed (o == b): carried as a clean entry.
    assert_eq!(r.clean_entries.len(), 1);
    assert_eq!(
        r.clean_entries[0].entry.unwrap().id,
        ObjectId::hash_blob(b"secret\n")
    );
}

/// Denies writes at or below `denied`.
struct DenyBelow(RepoPath);

impl WriteAuthorizer for DenyBelow {
    fn can_write(&self, path: &RepoPath) -> bool {
        !(self.0 == *path || self.0.is_ancestor_of(path))
    }
    fn can_import(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn write_authorization_on_delta_paths_against_a() {
    let env = env().await;
    let (_, a, b) = three_way(
        &env,
        &[("secret/s", file("s\n")), ("open/o", file("o\n"))],
        &[("secret/s", file("s by a\n"))],
        &[("open/o", file("o by b\n"))],
    )
    .await;
    let merge_as = |authz: DenyBelow| {
        let env = &env;
        async move {
            env.h
                .merge(
                    &a,
                    &b,
                    fields("m"),
                    change_id_for(&[a, b], "m"),
                    &alice(),
                    &authz,
                )
                .await
        }
    };
    // secret/ changed only on a: not in the delta, so writing it is not required.
    let m = merge_as(DenyBelow(p("secret"))).await.unwrap();
    assert_eq!(content_at(&env.h, &m, "open/o").await.unwrap(), b"o by b\n");
    // open/ is in the delta against a.
    let err = merge_as(DenyBelow(p("open"))).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
}

#[tokio::test]
async fn unrelated_roots_merge_against_the_empty_tree() {
    let env = env().await;
    let h = &env.h;
    let r1 = commit_delta(
        h,
        Vec::new(),
        vec![
            put_entry(h, "one/a", "a\n").await,
            put_entry(h, "x", "x1\n").await,
        ],
        "root one",
    )
    .await;
    let r2 = commit_delta(
        h,
        Vec::new(),
        vec![put_entry(h, "two/b", "b\n").await],
        "root two",
    )
    .await;
    assert!(h.merge_bases(&r1, &r2).await.unwrap().is_empty());
    let m = do_merge(&env, &r1, &r2).await.unwrap();
    let tree = snapshot(h, &root_of(h, &m).await).await;
    assert_eq!(tree.len(), 3);
    // The same path added on both roots with different content: add_add (base empty).
    let r3 = commit_delta(
        h,
        Vec::new(),
        vec![put_entry(h, "x", "x3\n").await],
        "root three",
    )
    .await;
    let err = do_merge(&env, &r1, &r3).await.unwrap_err();
    assert_eq!(
        kinds(report(&err)),
        vec![("x".to_owned(), ConflictKind::AddAdd)]
    );
    assert_ne!(root_of(h, &r1).await, empty_tree_id());
}

async fn put_entry(h: &scion_history::History, path: &str, c: &str) -> scion_object::DeltaEntry {
    let e = upload(h, scion_core::Mode::File, c.as_bytes()).await;
    scion_object::DeltaEntry {
        path: p(path),
        op: scion_object::DeltaOp::Put {
            mode: e.mode,
            id: e.id,
            size: e.size,
        },
    }
}

#[tokio::test]
async fn criss_cross_is_ambiguous() {
    let env = env().await;
    let h = &env.h;
    let base = commit(h, env.root, &[("f", file("f\n")), ("g", file("g\n"))]).await;
    let a1 = commit(h, base, &[("f", file("f-a1\n"))]).await;
    let b1 = commit(h, base, &[("g", file("g-b1\n"))]).await;
    let m1 = do_merge(&env, &a1, &b1).await.unwrap();
    let m2 = do_merge(&env, &b1, &a1).await.unwrap();
    assert_ne!(m1, m2);
    let a2 = commit(h, m1, &[("h", file("a2\n"))]).await;
    let b2 = commit(h, m2, &[("i", file("b2\n"))]).await;
    let bases = h.merge_bases(&a2, &b2).await.unwrap();
    assert_eq!(bases.len(), 2);
    let err = do_merge(&env, &a2, &b2).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::AmbiguousMergeBase);
}

#[tokio::test]
async fn ancestor_merges() {
    let env = env().await;
    let h = &env.h;
    let base = commit(h, env.root, &[("f", file("f\n"))]).await;
    let child = commit(h, base, &[("g", file("g\n"))]).await;
    // b is an ancestor of a: the merge takes a's tree with an empty delta.
    let m = do_merge(&env, &child, &base).await.unwrap();
    let rec = h.changeset(&m).await.unwrap();
    assert!(rec.changeset.delta.is_empty());
    assert_eq!(rec.changeset.root_tree, root_of(h, &child).await);
    // a is an ancestor of b: the delta is b's change.
    let m = do_merge(&env, &base, &child).await.unwrap();
    let rec = h.changeset(&m).await.unwrap();
    assert_eq!(rec.changeset.delta.len(), 1);
    assert_eq!(rec.changeset.root_tree, root_of(h, &child).await);
    // Unknown side.
    let missing = ObjectId::new(scion_core::Kind::Changeset, [3; 32]);
    let err = do_merge(&env, &base, &missing).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}
