//! SPEC 19.2 changeset validity and 19.13 redacted Puts: every SPEC 8.1 subcode, path rules,
//! limits, tree transitions, merge deltas, expected_root_tree and the server-set creator.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers outside #[test] fns

mod create_support;

use create_support::*;
use scion_core::{ErrorCode, ErrorPayload, Limits, Mode, ObjectId, ScionError, subcode};
use scion_history::{AllowAll, HistoryConfig, MetaSource};
use scion_object::{Canonical, DeltaEntry, DeltaOp, Extensions, RepoPath};
use scion_store::{ObjectState, RedactTxn};
use std::time::{Duration, SystemTime};

fn assert_invalid(r: Result<scion_history::Created, ScionError>, sub: &str) {
    let e = r.expect_err("expected INVALID_ARGUMENT");
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e}");
    assert_eq!(e.subcode(), Some(sub), "{e}");
}

fn assert_too_large(r: Result<scion_history::Created, ScionError>, name: &str) {
    let e = r.expect_err("expected TOO_LARGE");
    assert_eq!(e.code, ErrorCode::TooLarge, "{e}");
    match e.payload {
        ErrorPayload::Limit { name: n, .. } => assert_eq!(n, name),
        other => panic!("unexpected payload {other:?}"),
    }
}

/// Base: root -> c1 with `d/x`, `d/y`, `f`.
async fn base(e: &Env) -> ObjectId {
    let d = vec![
        put(&e.h, "d/x", "x1").await,
        put(&e.h, "d/y", "y1").await,
        put(&e.h, "f", "f1").await,
    ];
    create(&e.h, vec![e.root], d).await.changeset_id
}

async fn try_create(
    e: &Env,
    parents: Vec<ObjectId>,
    delta: Vec<DeltaEntry>,
) -> Result<scion_history::Created, ScionError> {
    e.h.create_changeset(req(parents, delta), &alice(), &AllowAll)
        .await
}

#[tokio::test]
async fn content_subcodes() {
    let e = env().await;
    let c1 = base(&e).await;

    // missing_blob
    assert_invalid(
        try_create(&e, vec![c1], vec![put_unstored("g", "never uploaded")]).await,
        subcode::MISSING_BLOB,
    );
    // size_mismatch
    let mut d = put(&e.h, "g", "four").await;
    if let DeltaOp::Put { size, .. } = &mut d.op {
        *size = 5;
    }
    assert_invalid(
        try_create(&e, vec![c1], vec![d]).await,
        subcode::SIZE_MISMATCH,
    );
    // noop_put
    assert_invalid(
        try_create(&e, vec![c1], vec![put(&e.h, "f", "f1").await]).await,
        subcode::NOOP_PUT,
    );
    // A mode change alone is not a no-op.
    let mut exec = put(&e.h, "f", "f1").await;
    if let DeltaOp::Put { mode, .. } = &mut exec.op {
        *mode = Mode::Executable;
    }
    try_create(&e, vec![c1], vec![exec]).await.unwrap();
    // delete_absent (absent path, and a path below a file)
    assert_invalid(
        try_create(&e, vec![c1], vec![del("nope")]).await,
        subcode::DELETE_ABSENT,
    );
    assert_invalid(
        try_create(&e, vec![c1], vec![del("f/below")]).await,
        subcode::DELETE_ABSENT,
    );
    // delete of a directory
    assert_invalid(
        try_create(&e, vec![c1], vec![del("d")]).await,
        subcode::DELETE_NOT_FILE,
    );
    // file -> directory without deleting the file
    assert_invalid(
        try_create(&e, vec![c1], vec![put(&e.h, "f/g", "g").await]).await,
        subcode::FILE_DIRECTORY,
    );
    // directory -> file without deleting every file below it
    assert_invalid(
        try_create(&e, vec![c1], vec![put(&e.h, "d", "d").await, del("d/x")]).await,
        subcode::FILE_DIRECTORY,
    );
    assert_invalid(
        try_create(&e, vec![c1], vec![put(&e.h, "d", "d").await]).await,
        subcode::FILE_DIRECTORY,
    );
    // delta_unsorted / delta_duplicate
    let a = put(&e.h, "a", "a").await;
    let b = put(&e.h, "b", "b").await;
    assert_invalid(
        try_create(&e, vec![c1], vec![b.clone(), a.clone()]).await,
        subcode::DELTA_UNSORTED,
    );
    assert_invalid(
        try_create(&e, vec![c1], vec![a.clone(), a.clone()]).await,
        subcode::DELTA_DUPLICATE,
    );
    assert_fsck_clean(&e.h).await;
}

#[tokio::test]
async fn parent_subcodes() {
    let e = env().await;
    let c1 = base(&e).await;
    let ghost = ObjectId::new(scion_core::Kind::Changeset, [9; 32]);
    assert_invalid(
        try_create(&e, vec![ghost], vec![]).await,
        subcode::MISSING_PARENT,
    );
    assert_invalid(
        try_create(&e, vec![c1, ghost], vec![]).await,
        subcode::MISSING_PARENT,
    );
    // A non-changeset id is not an existing parent.
    let tree_id = e.h.changeset(&c1).await.unwrap().changeset.root_tree;
    assert_invalid(
        try_create(&e, vec![tree_id], vec![]).await,
        subcode::MISSING_PARENT,
    );
    assert_invalid(
        try_create(&e, vec![c1, c1], vec![]).await,
        subcode::DUPLICATE_PARENT,
    );
    // Parentless after the root exists.
    assert_invalid(
        try_create(&e, vec![], vec![put(&e.h, "a", "a").await]).await,
        subcode::PARENTLESS_NOT_ALLOWED,
    );
    // Missing parent is reported before a bad delta (SPEC 8.1 order).
    assert_invalid(
        try_create(
            &e,
            vec![ghost],
            vec![put_unstored("b", "x"), put_unstored("a", "x")],
        )
        .await,
        subcode::MISSING_PARENT,
    );
}

#[tokio::test]
async fn import_mode_parentless() {
    let e = env().await;
    let delta = vec![put(&e.h, "a", "a").await];
    let mut r = req(vec![], delta.clone());
    r.import_mode = true;
    let denied = Deny {
        denied: None,
        import: false,
    };
    let err =
        e.h.create_changeset(r.clone(), &alice(), &denied)
            .await
            .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
    let ok =
        e.h.create_changeset(r.clone(), &alice(), &AllowAll)
            .await
            .unwrap();
    let rec = e.h.changeset(&ok.changeset_id).await.unwrap();
    assert!(rec.changeset.parents.is_empty());
    assert_eq!(rec.generation, 1);
    // Import mode with parents also needs can_import.
    let mut r2 = req(vec![e.root], delta);
    r2.import_mode = true;
    let err =
        e.h.create_changeset(r2, &alice(), &denied)
            .await
            .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
    assert_fsck_clean(&e.h).await;
}

#[tokio::test]
async fn path_rules() {
    // SPEC 4.4: paths are validated when parsed (proto conversion, CLI); every rule rejects.
    let long_name = vec![b'n'; 256];
    let mut long_path = Vec::new();
    while long_path.len() <= 4096 {
        long_path.extend_from_slice(b"abcdefgh/");
    }
    long_path.extend_from_slice(b"z");
    let bad: Vec<Vec<u8>> = vec![
        b".".to_vec(),
        b"a/./b".to_vec(),
        b"..".to_vec(),
        b"a/../b".to_vec(),
        b"/a".to_vec(),
        b"a/".to_vec(),
        b"a//b".to_vec(),
        b"a\0b".to_vec(),
        b".scion".to_vec(),
        b"x/.SCION/y".to_vec(),
        b".Scion".to_vec(),
        long_name,
        long_path,
    ];
    for b in bad {
        let err = RepoPath::parse(&b).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(err.subcode(), Some(subcode::INVALID_PATH));
    }
    assert!(RepoPath::parse(&[b'n'; 255]).is_ok());

    // The root path, a directory mode and a non-blob id are rejected by CreateChangeset.
    let e = env().await;
    let root_put = DeltaEntry {
        path: RepoPath::root(),
        op: DeltaOp::Put {
            mode: Mode::File,
            id: ObjectId::hash_blob(b"x"),
            size: 1,
        },
    };
    assert_invalid(
        try_create(&e, vec![e.root], vec![root_put]).await,
        subcode::INVALID_PATH,
    );
    let tree_mode = DeltaEntry {
        path: p("a"),
        op: DeltaOp::Put {
            mode: Mode::Tree,
            id: ObjectId::hash_blob(b"x"),
            size: 1,
        },
    };
    assert_invalid(
        try_create(&e, vec![e.root], vec![tree_mode]).await,
        subcode::INVALID_MODE,
    );
    let tree_id = DeltaEntry {
        path: p("a"),
        op: DeltaOp::Put {
            mode: Mode::File,
            id: scion_core::empty_tree_id(),
            size: 0,
        },
    };
    assert_invalid(
        try_create(&e, vec![e.root], vec![tree_id]).await,
        subcode::INVALID_KIND,
    );
}

#[tokio::test]
async fn limits() {
    let cfg = HistoryConfig {
        limits: Limits {
            max_parents: 2,
            max_delta_entries: 2,
            max_message_bytes: 16,
            ..Limits::default()
        },
        ..HistoryConfig::default()
    };
    let e = env_with(cfg).await;
    let a = create(&e.h, vec![e.root], vec![put(&e.h, "a", "a").await]).await;
    let b = create(&e.h, vec![e.root], vec![put(&e.h, "b", "b").await]).await;
    let mut c_req = req(vec![e.root], vec![put(&e.h, "c", "c").await]);
    c_req.change_id = scion_core::ChangeId([1; 16]);
    let c =
        e.h.create_changeset(c_req, &alice(), &AllowAll)
            .await
            .unwrap();
    // max_parents
    assert_too_large(
        try_create(
            &e,
            vec![a.changeset_id, b.changeset_id, c.changeset_id],
            vec![],
        )
        .await,
        "max_parents",
    );
    // max_delta_entries
    let d = vec![
        put(&e.h, "x", "1").await,
        put(&e.h, "y", "2").await,
        put(&e.h, "z", "3").await,
    ];
    assert_too_large(try_create(&e, vec![e.root], d).await, "max_delta_entries");
    // max_message_bytes
    let mut r = req(vec![e.root], vec![put(&e.h, "m", "m").await]);
    r.meta.message = vec![b'm'; 17];
    assert_too_large(
        e.h.create_changeset(r.clone(), &alice(), &AllowAll).await,
        "max_message_bytes",
    );
    r.meta.message = vec![b'm'; 16];
    e.h.create_changeset(r, &alice(), &AllowAll).await.unwrap();
}

#[tokio::test]
async fn permission_denied_on_any_delta_path() {
    let e = env().await;
    let authz = Deny {
        denied: Some(p("secret")),
        import: false,
    };
    let delta = vec![put(&e.h, "ok", "1").await, put(&e.h, "secret/k", "2").await];
    let err =
        e.h.create_changeset(req(vec![e.root], delta), &alice(), &authz)
            .await
            .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
    // A file/directory error (step 6) comes before authorization (step 7).
    let c = create(&e.h, vec![e.root], vec![put(&e.h, "secret", "s").await]).await;
    let err =
        e.h.create_changeset(
            req(vec![c.changeset_id], vec![put(&e.h, "secret/k", "2").await]),
            &alice(),
            &authz,
        )
        .await
        .unwrap_err();
    assert_eq!(err.subcode(), Some(subcode::FILE_DIRECTORY));
}

#[tokio::test]
async fn transitions_prune_and_empty_root() {
    let e = env().await;
    let c1 = base(&e).await;
    // Deleting the last files of `d` removes the directory.
    let c2 = create(&e.h, vec![c1], vec![del("d/x"), del("d/y")]).await;
    assert!(e.h.lookup(&c2.root_tree, &p("d")).await.unwrap().is_none());
    let t = e.h.tree(&c2.root_tree).await.unwrap();
    assert_eq!(t.entries.len(), 1);
    // File -> directory and directory -> file with the required deletes.
    let c3 = create(
        &e.h,
        vec![c1],
        vec![
            put(&e.h, "d", "now a file").await,
            del("d/x"),
            del("d/y"),
            del("f"),
            put(&e.h, "f/inner", "now a dir").await,
        ],
    )
    .await;
    assert_eq!(
        e.h.lookup(&c3.root_tree, &p("d"))
            .await
            .unwrap()
            .unwrap()
            .mode,
        Mode::File
    );
    assert_eq!(
        e.h.lookup(&c3.root_tree, &p("f"))
            .await
            .unwrap()
            .unwrap()
            .mode,
        Mode::Tree
    );
    // The root may become empty.
    let c4 = create(&e.h, vec![c2.changeset_id], vec![del("f")]).await;
    assert_eq!(c4.root_tree, scion_core::empty_tree_id());
    // file_diff reproduces each delta exactly.
    for (from, to) in [(c1, c3.changeset_id), (c2.changeset_id, c4.changeset_id)] {
        let a = e.h.changeset(&from).await.unwrap().changeset.root_tree;
        let rec = e.h.changeset(&to).await.unwrap();
        let diff =
            e.h.file_diff(Some(&a), &rec.changeset.root_tree, None)
                .await
                .unwrap();
        let as_delta: Vec<DeltaEntry> = diff
            .into_iter()
            .map(|c| DeltaEntry {
                path: c.path,
                op: match c.new {
                    Some(n) => DeltaOp::Put {
                        mode: n.mode,
                        id: n.id,
                        size: n.size,
                    },
                    None => DeltaOp::Delete,
                },
            })
            .collect();
        assert_eq!(as_delta, rec.changeset.delta);
    }
    assert_fsck_clean(&e.h).await;
}

#[tokio::test]
async fn merge_delta_is_relative_to_first_parent() {
    let e = env().await;
    let a = create(&e.h, vec![e.root], vec![put(&e.h, "a", "a").await]).await;
    let mut rb = req(vec![e.root], vec![put(&e.h, "b", "b").await]);
    rb.change_id = scion_core::ChangeId([2; 16]);
    let b = e.h.create_changeset(rb, &alice(), &AllowAll).await.unwrap();
    let parents = vec![a.changeset_id, b.changeset_id];
    // Relative to `a`, adding `b` is valid.
    let m = create(&e.h, parents.clone(), vec![put(&e.h, "b", "b").await]).await;
    let rec = e.h.changeset(&m.changeset_id).await.unwrap();
    assert_eq!(rec.changeset.parents, parents);
    assert_eq!(rec.generation, 3);
    assert!(e.h.lookup(&m.root_tree, &p("a")).await.unwrap().is_some());
    // A delta relative to the second parent is wrong: `a` is a no-op against `a`.
    assert_invalid(
        try_create(&e, parents, vec![put(&e.h, "a", "a").await]).await,
        subcode::NOOP_PUT,
    );
    assert_fsck_clean(&e.h).await;
}

#[tokio::test]
async fn expected_root_tree_and_creator() {
    let e = env().await;
    let delta = vec![put(&e.h, "a", "a").await];
    let mut r = req(vec![e.root], delta.clone());
    r.expected_root_tree = Some(scion_core::empty_tree_id());
    assert_invalid(
        e.h.create_changeset(r.clone(), &alice(), &AllowAll).await,
        subcode::ROOT_TREE_MISMATCH,
    );
    let ok = create(&e.h, vec![e.root], delta.clone()).await;
    r.expected_root_tree = Some(ok.root_tree);
    let again = e.h.create_changeset(r, &alice(), &AllowAll).await.unwrap();
    assert_eq!(again, ok);

    // creator is the server-set principal (the request has no creator field at all).
    let meta = e.h.objects().get_meta(&ok.meta_id).await.unwrap();
    assert_eq!(meta.creator, b"github:alice".to_vec());
    let rec = e.h.changeset(&ok.changeset_id).await.unwrap();
    assert_eq!(rec.created_by, "github:alice");
    // Another principal sending identical fields gets a different Meta and changeset.
    let bob = scion_core::Principal::Service("ci".to_owned());
    let other =
        e.h.create_changeset(req(vec![e.root], delta), &bob, &AllowAll)
            .await
            .unwrap();
    assert_ne!(other.changeset_id, ok.changeset_id);
    let meta = e.h.objects().get_meta(&other.meta_id).await.unwrap();
    assert_eq!(meta.creator, b"service:ci".to_vec());
}

#[tokio::test]
async fn unknown_critical_meta_extension_is_rejected() {
    let e = env().await;
    let mut r = req(vec![e.root], vec![put(&e.h, "a", "a").await]);
    let mut ext = Extensions::new();
    if ext.insert(7, b"critical".to_vec()).is_ok() {
        r.meta.extensions = ext;
        let err =
            e.h.create_changeset(r, &alice(), &AllowAll)
                .await
                .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
    }
}

#[tokio::test]
async fn redacted_puts_fail_but_carried_entries_succeed() {
    let e = env().await;
    let secret = put(&e.h, "secret.txt", "password=hunter2").await;
    let c1 = create(
        &e.h,
        vec![e.root],
        vec![put(&e.h, "keep.txt", "k").await, secret.clone()],
    )
    .await;
    let DeltaOp::Put { id: secret_id, .. } = secret.op else {
        unreachable!()
    };
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_100);
    e.h.meta()
        .redact(RedactTxn {
            ids: vec![secret_id],
            reason: "leaked".to_owned(),
            actor: "github:admin".to_owned(),
            at: now,
            purge_after: now + Duration::from_secs(86_400),
        })
        .await
        .unwrap();
    let row = e.h.objects().stat(&secret_id).await.unwrap().unwrap();
    assert_eq!(row.state, ObjectState::Quarantined);

    // Re-introducing the redacted blob at another path fails REDACTED.
    let reintro = DeltaEntry {
        path: p("copy.txt"),
        op: secret.op.clone(),
    };
    let err = try_create(&e, vec![c1.changeset_id], vec![reintro])
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Redacted);
    // Missing blobs (step 3) are reported before redaction (step 4).
    let err = try_create(
        &e,
        vec![c1.changeset_id],
        vec![
            DeltaEntry {
                path: p("a.txt"),
                op: secret.op.clone(),
            },
            put_unstored("b.txt", "nope"),
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(err.subcode(), Some(subcode::MISSING_BLOB));

    // A changeset not touching the redacted file succeeds and carries it over unchanged.
    let c2 = create(
        &e.h,
        vec![c1.changeset_id],
        vec![put(&e.h, "keep.txt", "k2").await],
    )
    .await;
    let carried = e.h.lookup(&c2.root_tree, &p("secret.txt")).await.unwrap();
    assert_eq!(carried.unwrap().id, secret_id);
    // Deleting the redacted file is fine too.
    create(&e.h, vec![c2.changeset_id], vec![del("secret.txt")]).await;
    assert_fsck_clean(&e.h).await;
}

#[tokio::test]
async fn create_internal_reuses_meta_and_skips_authz() {
    let e = env().await;
    let c1 = create(&e.h, vec![e.root], vec![put(&e.h, "a", "a").await]).await;
    let c2 =
        e.h.create_internal(
            vec![c1.changeset_id],
            vec![put(&e.h, "b", "b").await],
            MetaSource::Existing(c1.meta_id),
            scion_core::ChangeId([0x42; 16]),
            &alice(),
        )
        .await
        .unwrap();
    assert_eq!(c2.meta_id, c1.meta_id);
    let rec = e.h.changeset(&c2.changeset_id).await.unwrap();
    assert_eq!(rec.changeset.meta, c1.meta_id);
    // Internal creation still validates the delta.
    let err =
        e.h.create_internal(
            vec![c2.changeset_id],
            vec![put(&e.h, "b", "b").await],
            MetaSource::New(fields("x")),
            scion_core::ChangeId([1; 16]),
            &alice(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.subcode(), Some(subcode::NOOP_PUT));
    // A missing Meta is not found.
    let ghost = ObjectId::hash_encoded(scion_core::Kind::Meta, b"none");
    let err =
        e.h.create_internal(
            vec![c2.changeset_id],
            vec![put(&e.h, "c", "c").await],
            MetaSource::Existing(ghost),
            scion_core::ChangeId([1; 16]),
            &alice(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    assert_fsck_clean(&e.h).await;
}

#[tokio::test]
async fn lookup_and_changeset_record() {
    let e = env().await;
    let c1 = base(&e).await;
    let rec = e.h.changeset(&c1).await.unwrap();
    assert_eq!(rec.id, c1);
    assert_eq!(rec.generation, 2);
    assert_eq!(rec.skip, vec![e.root]);
    let root = rec.changeset.root_tree;
    assert!(
        e.h.lookup(&root, &RepoPath::root())
            .await
            .unwrap()
            .is_none()
    );
    assert!(e.h.lookup(&root, &p("f/x")).await.unwrap().is_none());
    assert!(e.h.lookup(&root, &p("missing")).await.unwrap().is_none());
    let x = e.h.lookup(&root, &p("d/x")).await.unwrap().unwrap();
    assert_eq!(x.id, ObjectId::hash_blob(b"x1"));
    assert_eq!(x.size, 2);
    let d = e.h.lookup(&root, &p("d")).await.unwrap().unwrap();
    assert_eq!(d.mode, Mode::Tree);
    let tree = e.h.tree(&d.id).await.unwrap();
    assert!(!tree.encode().is_empty());
    // object_paths rows: every Put blob at its path and every new tree at its directory.
    let rows = e.h.meta().object_paths(&d.id, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, p("d"));
    assert_eq!(rows[0].first_changeset, c1);
    let rows = e.h.meta().object_paths(&root, 10).await.unwrap();
    assert_eq!(rows[0].path, RepoPath::root());
}
