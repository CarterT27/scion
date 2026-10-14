//! SPEC 19.4 / 19.13: one dedicated rebase test for every SPEC 8.3 table row and both
//! structural rules, the binary heuristic and `merge_max_text_bytes` boundaries, the redacted
//! rule, conflict payloads (clean_entries, base/ours/theirs, hunks), and the marker scan.
//!
//! Every test builds `P` (base), `X = P + ours` and `C = P + theirs` and rebases `C` onto `X`.
//! Row 2 (`t == b`) cannot occur for a delta path of `C`; it is covered by `merge_rules.rs`.

mod merge_support;

use merge_support::*;
use scion_core::{ConflictKind, ErrorCode, Mode, ObjectId};

struct Case {
    env: Env,
    p: ObjectId,
    x: ObjectId,
    c: ObjectId,
}

async fn case_with(
    env: Env,
    base: &[Change<'_>],
    ours: &[Change<'_>],
    theirs: &[Change<'_>],
) -> Case {
    let p = if base.is_empty() {
        env.root
    } else {
        commit(&env.h, env.root, base).await
    };
    let x = commit(&env.h, p, ours).await;
    let c = commit(&env.h, p, theirs).await;
    Case { env, p, x, c }
}

async fn case(base: &[Change<'_>], ours: &[Change<'_>], theirs: &[Change<'_>]) -> Case {
    case_with(env().await, base, ours, theirs).await
}

impl Case {
    async fn rebase(&self) -> scion_core::Result<ObjectId> {
        self.env.h.rebase(&self.c, &self.x, &alice()).await
    }

    async fn conflict(&self) -> scion_core::ConflictReport {
        let err = self.rebase().await.expect_err("conflict expected");
        report(&err).clone()
    }
}

#[tokio::test]
async fn row1_ours_equals_base_takes_theirs() {
    let k = case(
        &[("f", file("base\n")), ("g", file("g\n"))],
        &[("g", file("g2\n"))],
        &[("f", file("theirs\n"))],
    )
    .await;
    let r = k.rebase().await.unwrap();
    assert_eq!(content_at(&k.env.h, &r, "f").await.unwrap(), b"theirs\n");
    assert_eq!(content_at(&k.env.h, &r, "g").await.unwrap(), b"g2\n");
    let rec = k.env.h.changeset(&r).await.unwrap();
    assert_eq!(rec.changeset.parents, vec![k.x]);
    assert_eq!(rec.changeset.delta.len(), 1);
    assert_eq!(rec.changeset.delta[0].path, p("f"));
    // Row 1 with a delete on theirs.
    let k = case(
        &[("f", file("base\n")), ("g", file("g\n"))],
        &[("g", file("g2\n"))],
        &[("f", DEL)],
    )
    .await;
    let r = k.rebase().await.unwrap();
    assert!(entry_at(&k.env.h, &r, "f").await.is_none());
    assert_no_markers(&k.env).await;
}

#[tokio::test]
async fn row3_ours_equals_theirs_takes_ours() {
    // The same edit on both sides plus an extra theirs path: f resolves to o (not in the
    // delta), g is applied.
    let k = case(
        &[("f", file("base\n"))],
        &[("f", file("same\n"))],
        &[("f", file("same\n")), ("g", file("new\n"))],
    )
    .await;
    let r = k.rebase().await.unwrap();
    let rec = k.env.h.changeset(&r).await.unwrap();
    let paths: Vec<_> = rec.changeset.delta.iter().map(|e| e.path.clone()).collect();
    assert_eq!(paths, vec![p("g")]);
    assert_eq!(content_at(&k.env.h, &r, "f").await.unwrap(), b"same\n");
    // Both deleted the same file: also row 3.
    let k = case(
        &[("f", file("base\n")), ("h", file("h\n"))],
        &[("f", DEL)],
        &[("f", DEL), ("g", file("g\n"))],
    )
    .await;
    let r = k.rebase().await.unwrap();
    assert!(entry_at(&k.env.h, &r, "f").await.is_none());
    assert!(entry_at(&k.env.h, &r, "g").await.is_some());
}

#[tokio::test]
async fn add_add_conflict() {
    let k = case(
        &[("other", file("o\n"))],
        &[("f", file("ours\n"))],
        &[("f", file("theirs\n"))],
    )
    .await;
    let r = k.conflict().await;
    assert_eq!(kinds(&r), vec![("f".to_owned(), ConflictKind::AddAdd)]);
    let c = &r.conflicts[0];
    assert!(c.base.is_none());
    assert_eq!(c.ours.unwrap().id, ObjectId::hash_blob(b"ours\n"));
    assert_eq!(c.theirs.unwrap().id, ObjectId::hash_blob(b"theirs\n"));
    assert!(c.hunks.is_empty());
}

#[tokio::test]
async fn modify_delete_conflict_both_directions() {
    let k = case(
        &[("f", file("base\n"))],
        &[("f", DEL)],
        &[("f", file("theirs\n"))],
    )
    .await;
    let r = k.conflict().await;
    assert_eq!(
        kinds(&r),
        vec![("f".to_owned(), ConflictKind::ModifyDelete)]
    );
    assert!(r.conflicts[0].ours.is_none());
    assert_eq!(
        r.conflicts[0].base.unwrap().id,
        ObjectId::hash_blob(b"base\n")
    );

    let k = case(
        &[("f", file("base\n"))],
        &[("f", file("ours\n"))],
        &[("f", DEL)],
    )
    .await;
    let r = k.conflict().await;
    assert_eq!(
        kinds(&r),
        vec![("f".to_owned(), ConflictKind::ModifyDelete)]
    );
    assert!(r.conflicts[0].theirs.is_none());
}

#[tokio::test]
async fn symlink_conflict() {
    // All three symlinks.
    let k = case(
        &[("l", link("target-base"))],
        &[("l", link("target-ours"))],
        &[("l", link("target-theirs"))],
    )
    .await;
    assert_eq!(
        kinds(&k.conflict().await),
        vec![("l".to_owned(), ConflictKind::Symlink)]
    );
    // Only theirs is a symlink.
    let k = case(
        &[("l", file("a\n"))],
        &[("l", file("b\n"))],
        &[("l", link("c"))],
    )
    .await;
    assert_eq!(
        kinds(&k.conflict().await),
        vec![("l".to_owned(), ConflictKind::Symlink)]
    );
    // Only the base is a symlink (amendment W1-1).
    let k = case(
        &[("l", link("a"))],
        &[("l", file("b\n"))],
        &[("l", file("c\n"))],
    )
    .await;
    assert_eq!(
        kinds(&k.conflict().await),
        vec![("l".to_owned(), ConflictKind::Symlink)]
    );
}

/// Text of `len` bytes made of distinct 10-byte `\n`-terminated lines (the last one possibly
/// partial and unterminated), with a NUL at `nul` if given.
fn text(len: usize, fill: u8, nul: Option<usize>) -> Vec<u8> {
    let mut v: Vec<u8> = Vec::with_capacity(len + 10);
    let mut i = 0usize;
    while v.len() < len {
        v.extend_from_slice(format!("{}{i:07}x\n", fill as char).as_bytes());
        i += 1;
    }
    v.truncate(len);
    if let Some(i) = nul {
        v[i] = 0;
    }
    v
}

/// Base/ours/theirs where ours edits the first line and theirs the last (a clean merge).
fn three(len: usize, theirs_nul: Option<usize>) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let base = text(len, b'b', None);
    let mut ours = base.clone();
    ours[0] = b'o';
    let mut theirs = text(len, b'b', theirs_nul);
    let last = len - 2;
    theirs[last] = b't';
    (base, ours, theirs)
}

#[tokio::test]
async fn binary_heuristic_boundary() {
    // NUL at byte 7999: binary.
    let (b, o, t) = three(9000, Some(7999));
    let k = case(
        &[("f", raw(Mode::File, &b))],
        &[("f", raw(Mode::File, &o))],
        &[("f", raw(Mode::File, &t))],
    )
    .await;
    assert_eq!(
        kinds(&k.conflict().await),
        vec![("f".to_owned(), ConflictKind::Binary)]
    );
    // NUL at byte 8000: text, merges cleanly.
    let (b, o, t) = three(9000, Some(8000));
    let k = case(
        &[("f", raw(Mode::File, &b))],
        &[("f", raw(Mode::File, &o))],
        &[("f", raw(Mode::File, &t))],
    )
    .await;
    let r = k.rebase().await.unwrap();
    let merged = content_at(&k.env.h, &r, "f").await.unwrap();
    assert_eq!(merged[0], b'o');
    assert_eq!(merged[8000], 0);
    assert_eq!(merged[9000 - 2], b't');
    // A binary base with text sides is binary too.
    let k = case(
        &[("f", raw(Mode::File, b"a\0\n"))],
        &[("f", file("o\n"))],
        &[("f", file("t\n"))],
    )
    .await;
    assert_eq!(
        kinds(&k.conflict().await),
        vec![("f".to_owned(), ConflictKind::Binary)]
    );
}

#[tokio::test]
async fn merge_max_text_bytes_boundary() {
    const MAX: u64 = 64;
    // Every side exactly at the limit: content merge.
    let (b, o, t) = three(64, None);
    let k = case_with(
        env_with(cfg_max_text(MAX)).await,
        &[("f", raw(Mode::File, &b))],
        &[("f", raw(Mode::File, &o))],
        &[("f", raw(Mode::File, &t))],
    )
    .await;
    let r = k.rebase().await.unwrap();
    let merged = content_at(&k.env.h, &r, "f").await.unwrap();
    assert_eq!(merged.len(), 64);
    assert_eq!((merged[0], merged[62]), (b'o', b't'));

    // One side at limit + 1: binary.
    let mut t1 = t.clone();
    t1.push(b'x');
    let k = case_with(
        env_with(cfg_max_text(MAX)).await,
        &[("f", raw(Mode::File, &b))],
        &[("f", raw(Mode::File, &o))],
        &[("f", raw(Mode::File, &t1))],
    )
    .await;
    assert_eq!(
        kinds(&k.conflict().await),
        vec![("f".to_owned(), ConflictKind::Binary)]
    );
    // The base above the limit: binary as well.
    let mut b1 = b.clone();
    b1.push(b'x');
    let k = case_with(
        env_with(cfg_max_text(MAX)).await,
        &[("f", raw(Mode::File, &b1))],
        &[("f", raw(Mode::File, &o))],
        &[("f", raw(Mode::File, &t))],
    )
    .await;
    assert_eq!(
        kinds(&k.conflict().await),
        vec![("f".to_owned(), ConflictKind::Binary)]
    );
}

#[tokio::test]
async fn redacted_content_conflict() {
    // 19.13: a rebase needing redacted (quarantined) content gives a redacted conflict.
    let k = case(
        &[("f", file("a\nb\nc\n"))],
        &[("f", file("A\nb\nc\n"))],
        &[("f", file("a\nb\nC\n"))],
    )
    .await;
    redact(&k.env, &[ObjectId::hash_blob(b"a\nb\nc\n")]).await;
    let r = k.conflict().await;
    assert_eq!(kinds(&r), vec![("f".to_owned(), ConflictKind::Redacted)]);
    assert_eq!(
        r.conflicts[0].base.unwrap().id,
        ObjectId::hash_blob(b"a\nb\nc\n")
    );

    // A purged ours side.
    let k = case(
        &[("f", file("a\nb\nc\n"))],
        &[("f", file("A\nb\nc\n"))],
        &[("f", file("a\nb\nC\n"))],
    )
    .await;
    let ours = ObjectId::hash_blob(b"A\nb\nc\n");
    redact(&k.env, &[ours]).await;
    purge(&k.env, &[ours]).await;
    assert_eq!(
        kinds(&k.conflict().await),
        vec![("f".to_owned(), ConflictKind::Redacted)]
    );

    // Redacted content that is NOT needed (o == b) is carried without a conflict.
    let k = case(
        &[("f", file("a\n")), ("g", file("g\n"))],
        &[("g", file("g2\n"))],
        &[("f", file("secret\n"))],
    )
    .await;
    redact(&k.env, &[ObjectId::hash_blob(b"secret\n")]).await;
    let r = k.rebase().await.unwrap();
    assert_eq!(
        entry_at(&k.env.h, &r, "f").await.unwrap().id,
        ObjectId::hash_blob(b"secret\n")
    );
}

#[tokio::test]
async fn content_clean_merge() {
    let k = case(
        &[("dir/f", file("1\n2\n3\n4\n5\n6\n"))],
        &[("dir/f", file("one\n2\n3\n4\n5\n6\n"))],
        &[("dir/f", file("1\n2\n3\n4\n5\nsix\n"))],
    )
    .await;
    let r = k.rebase().await.unwrap();
    let merged = b"one\n2\n3\n4\n5\nsix\n";
    assert_eq!(content_at(&k.env.h, &r, "dir/f").await.unwrap(), merged);
    let e = entry_at(&k.env.h, &r, "dir/f").await.unwrap();
    assert_eq!(e.id, ObjectId::hash_blob(merged));
    assert_eq!(e.size, merged.len() as u64);
    assert_eq!(e.mode, Mode::File);
    let rec = k.env.h.changeset(&r).await.unwrap();
    assert_eq!(rec.changeset.delta.len(), 1);
    // Unterminated final lines and CRLF merge byte-exactly.
    let k = case(
        &[("f", file("a\r\nb\r\nc\r\nd\r\ne"))],
        &[("f", file("A\r\nb\r\nc\r\nd\r\ne"))],
        &[("f", file("a\r\nb\r\nc\r\nd\r\nE"))],
    )
    .await;
    let r = k.rebase().await.unwrap();
    assert_eq!(
        content_at(&k.env.h, &r, "f").await.unwrap(),
        b"A\r\nb\r\nc\r\nd\r\nE"
    );
    assert_no_markers(&k.env).await;
}

#[tokio::test]
async fn content_conflict_with_hunks() {
    let k = case(
        &[("f", file("1\n2\n3\n"))],
        &[("f", file("1\nours\n3\n"))],
        &[("f", file("1\ntheirs\n3\n"))],
    )
    .await;
    let r = k.conflict().await;
    assert_eq!(kinds(&r), vec![("f".to_owned(), ConflictKind::Content)]);
    let c = &r.conflicts[0];
    assert_eq!(c.base.unwrap().id, ObjectId::hash_blob(b"1\n2\n3\n"));
    assert_eq!(c.ours.unwrap().id, ObjectId::hash_blob(b"1\nours\n3\n"));
    assert_eq!(c.theirs.unwrap().id, ObjectId::hash_blob(b"1\ntheirs\n3\n"));
    assert_eq!(c.hunks.len(), 1);
    assert_eq!(c.hunks[0].base, 1..2);
    assert_eq!(c.hunks[0].ours, 1..2);
    assert_eq!(c.hunks[0].theirs, 1..2);
    // Nothing with markers was stored.
    assert_no_markers(&k.env).await;
}

#[tokio::test]
async fn mode_merge() {
    // Ours flips the mode, theirs edits the content: the merged entry has both.
    let k = case(
        &[("s", file("a\nb\nc\n"))],
        &[("s", exec("a\nb\nc\n"))],
        &[("s", file("a\nb\nC\n"))],
    )
    .await;
    let r = k.rebase().await.unwrap();
    let e = entry_at(&k.env.h, &r, "s").await.unwrap();
    assert_eq!(e.mode, Mode::Executable);
    assert_eq!(e.id, ObjectId::hash_blob(b"a\nb\nC\n"));
    // Theirs flips the mode, ours edits the content.
    let k = case(
        &[("s", file("a\nb\nc\n"))],
        &[("s", file("A\nb\nc\n"))],
        &[("s", exec("a\nb\nc\n"))],
    )
    .await;
    let r = k.rebase().await.unwrap();
    let e = entry_at(&k.env.h, &r, "s").await.unwrap();
    assert_eq!(e.mode, Mode::Executable);
    assert_eq!(e.id, ObjectId::hash_blob(b"A\nb\nc\n"));
}

#[tokio::test]
async fn structural_put_file_where_ours_has_directory() {
    let k = case(
        &[("keep", file("k\n"))],
        &[("d/x", file("x\n"))],
        &[("d", file("file\n"))],
    )
    .await;
    let r = k.conflict().await;
    assert_eq!(
        kinds(&r),
        vec![("d".to_owned(), ConflictKind::FileDirectory)]
    );
    assert!(r.conflicts[0].ours.is_none());
    assert!(r.conflicts[0].theirs.is_some());
}

#[tokio::test]
async fn structural_put_file_below_ours_file() {
    let k = case(
        &[("keep", file("k\n"))],
        &[("d", file("file\n"))],
        &[("d/x", file("x\n"))],
    )
    .await;
    let r = k.conflict().await;
    assert_eq!(
        kinds(&r),
        vec![("d/x".to_owned(), ConflictKind::FileDirectory)]
    );
    // Deeper: a file at a grand-ancestor.
    let k = case(
        &[("keep", file("k\n"))],
        &[("a", file("file\n"))],
        &[("a/b/c", file("x\n"))],
    )
    .await;
    assert_eq!(
        kinds(&k.conflict().await),
        vec![("a/b/c".to_owned(), ConflictKind::FileDirectory)]
    );
}

#[tokio::test]
async fn structural_delete_where_ours_has_directory() {
    let k = case(
        &[("d", file("file\n")), ("keep", file("k\n"))],
        &[("d", DEL), ("d/x", file("x\n"))],
        &[("d", DEL)],
    )
    .await;
    let r = k.conflict().await;
    assert_eq!(
        kinds(&r),
        vec![("d".to_owned(), ConflictKind::FileDirectory)]
    );
    let c = &r.conflicts[0];
    assert_eq!(c.base.unwrap().id, ObjectId::hash_blob(b"file\n"));
    assert!(c.ours.is_none() && c.theirs.is_none());
}

#[tokio::test]
async fn every_conflict_is_collected_with_clean_entries() {
    let k = case(
        &[
            ("a", file("1\n2\n3\n")),
            ("b", file("base\n")),
            ("c", file("1\n2\n3\n4\n5\n")),
            ("u", file("u\n")),
        ],
        &[
            ("a", file("1\nours\n3\n")),
            ("b", DEL),
            ("c", file("one\n2\n3\n4\n5\n")),
        ],
        &[
            ("a", file("1\ntheirs\n3\n")),
            ("b", file("theirs\n")),
            ("c", file("1\n2\n3\n4\nfive\n")),
            ("n", file("new\n")),
            ("u", DEL),
        ],
    )
    .await;
    let r = k.conflict().await;
    assert_eq!(
        kinds(&r),
        vec![
            ("a".to_owned(), ConflictKind::Content),
            ("b".to_owned(), ConflictKind::ModifyDelete),
        ]
    );
    let clean: Vec<(String, Option<ObjectId>)> = r
        .clean_entries
        .iter()
        .map(|e| {
            (
                String::from_utf8_lossy(&e.path).into_owned(),
                e.entry.map(|e| e.id),
            )
        })
        .collect();
    let merged_c = ObjectId::hash_blob(b"one\n2\n3\n4\nfive\n");
    assert_eq!(
        clean,
        vec![
            ("c".to_owned(), Some(merged_c)),
            ("n".to_owned(), Some(ObjectId::hash_blob(b"new\n"))),
            ("u".to_owned(), None),
        ]
    );
    // The merged blob named by clean_entries is stored (clients write clean entries).
    assert_eq!(
        k.env.h.objects().get(&merged_c).await.unwrap().as_ref(),
        b"one\n2\n3\n4\nfive\n"
    );
    // The ref-free history is unchanged: no changeset was created for the failed rebase.
    let children = k.env.h.meta().children(&k.x).await.unwrap();
    assert!(children.is_empty());
    assert_eq!(
        k.p,
        k.env.h.changeset(&k.c).await.unwrap().changeset.parents[0]
    );
    assert_no_markers(&k.env).await;
}

#[tokio::test]
async fn rebase_error_codes() {
    let k = case(
        &[("f", file("1\n"))],
        &[("g", file("g\n"))],
        &[("f", file("2\n"))],
    )
    .await;
    // Unknown onto.
    let missing = ObjectId::new(scion_core::Kind::Changeset, [9; 32]);
    let err = k.env.h.rebase(&k.c, &missing, &alice()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    let err = k.env.h.rebase(&missing, &k.x, &alice()).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}
