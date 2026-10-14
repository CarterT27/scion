//! SPEC 17 / 19.10 checkout path safety: malicious trees served by a fake server are refused
//! before any write, pre-existing symlinked parents are not followed, symlinks are never
//! followed while writing, and case collisions are reported rather than overwritten (SPEC 4.4,
//! PLAN 6.17) (P18).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod checkout_support;

use checkout_support::*;
use scion_core::{ErrorCode, Mode, ObjectId};
use scion_wc::{WcOptions, WorkingCopy};

/// Serve a root tree containing `ok.txt` and a subtree `sub` that holds an entry named `bad`
/// (hand-encoded, bypassing validation).
fn malicious_env_tree(env: &Env, bad: &[u8], bad_mode: Mode) -> ObjectId {
    let blob = ObjectId::hash_blob(b"payload\n");
    env.up
        .insert_object(blob, bytes::Bytes::from_static(b"payload\n"), false);
    let inner_file = ObjectId::hash_blob(b"inner\n");
    env.up
        .insert_object(inner_file, bytes::Bytes::from_static(b"inner\n"), false);
    let (inner_tree, inner_raw) = raw_tree(&[(b"x.txt", Mode::File, inner_file, 6)]);
    env.up.insert_object(inner_tree, inner_raw, false);
    let (bad_id, bad_size) = if bad_mode == Mode::Tree {
        (inner_tree, 0)
    } else {
        (blob, 8)
    };
    let (sub, sub_raw) = raw_tree(&[(bad, bad_mode, bad_id, bad_size)]);
    env.up.insert_object(sub, sub_raw, false);
    let (root, root_raw) = raw_tree(&[
        (b"ok.txt", Mode::File, blob, 8),
        (b"sub", Mode::Tree, sub, 0),
    ]);
    env.up.insert_object(root, root_raw, false);
    let cs = env.changeset(root);
    env.up.set_ref("main", cs, 1);
    cs
}

#[tokio::test]
async fn malicious_names_are_refused_before_any_write() {
    let cases: &[(&[u8], Mode)] = &[
        (b"..", Mode::Tree),
        (b"..", Mode::File),
        (b".", Mode::File),
        (b".scion", Mode::Tree),
        (b".scion", Mode::File),
        (b".SCION", Mode::File),
        (b".ScIoN", Mode::Tree),
        (b"a/b", Mode::File),
        (b"nul\0byte", Mode::File),
        (b"", Mode::File),
    ];
    for (name, mode) in cases {
        let env = Env::start().await;
        malicious_env_tree(&env, name, *mode);
        let outside = tmp();
        let dir = tmp();
        let root = dir.path().join("wc");
        std::fs::create_dir(&root).unwrap();
        let err = WorkingCopy::checkout(
            env.client.clone(),
            &root,
            "main",
            Vec::new(),
            WcOptions::default(),
        )
        .await
        .err()
        .unwrap_or_else(|| panic!("tree with {name:?} must be refused"));
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{name:?}: {err}");
        assert!(
            ls(&root).is_empty(),
            "{name:?}: nothing written (not even .scion): {:?}",
            ls(&root)
        );
        assert!(ls(dir.path()) == vec!["wc".to_owned()]);
        assert!(ls(outside.path()).is_empty());
    }
}

#[tokio::test]
async fn overlong_paths_are_refused_before_any_write() {
    let env = Env::start().await;
    let name = vec![b'n'; 255];
    let blob = ObjectId::hash_blob(b"x");
    env.up
        .insert_object(blob, bytes::Bytes::from_static(b"x"), false);
    // 17 nested 255-byte names exceed the 4096-byte path limit.
    let (mut id, raw) = raw_tree(&[(b"f", Mode::File, blob, 1)]);
    env.up.insert_object(id, raw, false);
    for _ in 0..17 {
        let (t, raw) = raw_tree(&[(&name, Mode::Tree, id, 0)]);
        env.up.insert_object(t, raw, false);
        id = t;
    }
    let (root, raw) = raw_tree(&[
        (b"ok.txt", Mode::File, blob, 1),
        (b"deep", Mode::Tree, id, 0),
    ]);
    env.up.insert_object(root, raw, false);
    let cs = env.changeset(root);
    env.up.set_ref("main", cs, 1);
    let dir = tmp();
    let root_dir = dir.path().join("wc");
    let err = WorkingCopy::checkout(
        env.client.clone(),
        &root_dir,
        "main",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(ls(&root_dir).is_empty());
}

#[tokio::test]
async fn pre_existing_symlinked_parent_is_not_followed() {
    let env = Env::start().await;
    env.commit(
        &files(&[("a.txt", 1, "a\n"), ("sub/file.txt", 1, "escape\n")]),
        "main",
    );
    let outside = tmp();
    let dir = tmp();
    let root = dir.path().join("wc");
    std::fs::create_dir(&root).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("sub")).unwrap();
    let err = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert!(
        ls(outside.path()).is_empty(),
        "nothing written through the symlink"
    );
    assert_eq!(
        ls(&root),
        vec!["sub".to_owned()],
        "refused before any write"
    );
}

#[tokio::test]
async fn symlinks_are_never_followed_by_goto() {
    let env = Env::start().await;
    let outside = tmp();
    let target = outside.path().to_str().unwrap().to_owned();
    // v1: `d` is a symlink pointing outside; v2: `d` is a real directory with a file.
    env.commit(&files(&[("d", 3, &target), ("k.txt", 1, "k\n")]), "v1");
    env.commit(
        &files(&[("d/pwned.txt", 1, "inside\n"), ("k.txt", 1, "k\n")]),
        "v2",
    );
    let dir = tmp();
    let root = dir.path().join("wc");
    let mut wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "v1",
        Vec::new(),
        WcOptions::default(),
    )
    .await
    .unwrap();
    assert!(
        std::fs::symlink_metadata(root.join("d"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    wc.goto("v2", false).await.unwrap();
    assert!(
        ls(outside.path()).is_empty(),
        "goto wrote through a symlink"
    );
    assert!(root.join("d").is_dir());
    assert_eq!(
        std::fs::read(root.join("d/pwned.txt")).unwrap(),
        b"inside\n"
    );
    // And back: the directory becomes a symlink again; the outside stays untouched.
    wc.goto("v1", false).await.unwrap();
    assert!(
        std::fs::symlink_metadata(root.join("d"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(ls(outside.path()).is_empty());
}

#[tokio::test]
async fn symlink_traversal_through_case_folding_is_not_followed() {
    let env = Env::start().await;
    let outside = tmp();
    let target = outside.path().to_str().unwrap().to_owned();
    env.commit(
        &files(&[("LINK/pwned.txt", 1, "x\n"), ("link", 3, &target)]),
        "main",
    );
    let dir = tmp();
    let root = dir.path().join("wc");
    let wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        Vec::new(),
        WcOptions {
            force_case_insensitive: true,
            ..WcOptions::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(wc.collisions(), &[(rp("LINK/pwned.txt"), rp("link"))]);
    assert!(ls(outside.path()).is_empty());
    assert!(std::fs::symlink_metadata(root.join("link")).is_err() || cfg!(target_os = "macos"));
}

async fn collision_case(force: bool) -> Option<(tempfile::TempDir, WorkingCopy)> {
    let env = Env::start().await;
    env.commit(
        &files(&[
            ("Dir/x.txt", 1, "upper dir\n"),
            ("README", 1, "upper\n"),
            ("dir/y.txt", 1, "lower dir\n"),
            ("other.txt", 1, "o\n"),
            ("readme", 1, "lower\n"),
        ]),
        "main",
    );
    let dir = tmp();
    let root = dir.path().join("wc");
    let wc = WorkingCopy::checkout(
        env.client.clone(),
        &root,
        "main",
        Vec::new(),
        WcOptions {
            force_case_insensitive: force,
            ..WcOptions::default()
        },
    )
    .await
    .unwrap();
    if wc.collisions().is_empty() {
        return None;
    }
    assert_eq!(
        wc.collisions(),
        &[
            (rp("Dir/x.txt"), rp("dir/y.txt")),
            (rp("README"), rp("readme")),
        ]
    );
    // The first entry of each pair is written and never overwritten by the second.
    let snap = scion_wc::testing::snapshot(&root).unwrap();
    let names: Vec<String> = snap
        .keys()
        .map(|p| String::from_utf8_lossy(p.as_bytes()).into_owned())
        .collect();
    assert_eq!(names, vec!["Dir/x.txt", "README", "other.txt"]);
    assert_eq!(std::fs::read(root.join("README")).unwrap(), b"upper\n");
    Some((dir, wc))
}

#[tokio::test]
async fn case_collisions_are_reported_not_overwritten_when_forced() {
    assert!(collision_case(true).await.is_some());
}

#[tokio::test]
async fn case_collisions_on_a_real_case_insensitive_filesystem() {
    let probe = tmp();
    let safe = scion_wc::fs_safe::SafeRoot::open(probe.path()).unwrap();
    if !safe.is_case_insensitive() {
        eprintln!("SKIP: the temp filesystem is case-sensitive");
        return;
    }
    assert!(
        collision_case(false).await.is_some(),
        "collisions detected by probing the filesystem"
    );
}
