//! SPEC 19.14 edge cases on the synthetic edge history and git fixtures (P29): octopus
//! merges, root commits, multiple roots, empty commits, mode 100664, symlinks, executable
//! flips, gitlinks (failing without `--skip-submodules`, recorded in Meta extension tag 4
//! with it), incremental import with fast-forward, idempotent re-import, importing into the
//! bootstrap `main`, refusing non-fast-forward targets, rate-limit retry, upload batching.

mod import_support;

use import_support::*;
use scion_core::{ErrorCode, Mode, ObjectId, empty_tree_id};
use scion_import::testing::{
    GitCommitView, ScionReader, check_fidelity, git_commits, options, sha_hex,
};
use scion_object::{DeltaOp, META_EXT_SKIPPED_GITLINKS};
use scion_proto::v1;
use scion_testkit::git::{GitFixture, GitOp, git_available};

const GITLINK_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn by_subject<'a>(commits: &'a [GitCommitView], subject: &str) -> &'a GitCommitView {
    commits
        .iter()
        .find(|c| c.message.split(|b| *b == b'\n').next() == Some(subject.as_bytes()))
        .unwrap_or_else(|| panic!("no commit with subject {subject:?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn gitlink_fails_without_skip_submodules() {
    let s = server().await;
    let c = importer_client(&s).await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = unpack("synthetic-edge", tmp.path());
    let r = scion_import::import(&c, &options(&repo, "main", "imp/nogitlink", false)).await;
    let e = r.expect_err("gitlink must fail the import");
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert!(e.message.contains("vendor/sub"), "{e}");
    assert!(e.message.contains("--skip-submodules"), "{e}");
    // The ref is written only after a complete walk.
    assert!(is_code(
        &resolve_ref(&c, "imp/nogitlink").await,
        ErrorCode::NotFound
    ));
    // The commit adding the gitlink was not imported; its parent was.
    let commits = git_commits(&repo, "main").unwrap();
    let gl = by_subject(&commits, "add gitlink vendor/sub");
    assert!(
        scion_import::testing::resolve_git(&c, &gl.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        scion_import::testing::resolve_git(&c, &gl.parents[0])
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn synthetic_edge_history_into_bootstrap_main() {
    let s = server().await;
    let c = importer_client(&s).await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = unpack("synthetic-edge", tmp.path());
    let commits = git_commits(&repo, "main").unwrap();

    // `main` exists at the bootstrap root (version 1): the import replaces it.
    let (root, v1_) = resolve_ref(&c, "main").await.unwrap();
    assert_eq!(v1_, 1);
    let rep = scion_import::import(&c, &options(&repo, "main", "main", true))
        .await
        .unwrap();
    assert_eq!(rep.commits_seen, commits.len() as u64);
    assert_eq!(rep.imported, commits.len() as u64);
    assert_eq!(rep.skipped_existing, 0);
    assert!(rep.blobs_uploaded > 0);
    assert_eq!(
        rep.skipped_gitlinks,
        vec![("vendor/sub".to_owned(), GITLINK_SHA.to_owned())]
    );
    assert_eq!(rep.ref_version, 2);
    let (target, version) = resolve_ref(&c, "main").await.unwrap();
    assert_eq!(Some(target), rep.final_target);
    assert_eq!(version, 2);
    assert_ne!(target, root);
    let tip = commits.last().unwrap();
    assert_eq!(target, mapped(&c, &tip.id).await);

    let f = check_fidelity(&c, &repo, "main").await.unwrap();
    assert!(f.mismatches.is_empty(), "{:?}", f.mismatches);
    assert!(f.gitlinks_excluded > 0);

    let reader = ScionReader::new(&c);
    let cs_of = |subject: &str| by_subject(&commits, subject).id.clone();

    // Root commit: parentless; 100664 -> File, 100755 -> Executable, symlink -> Symlink.
    let root_cs = mapped(&c, &cs_of("root: initial files")).await;
    let root_changeset = reader.changeset(&root_cs).await.unwrap();
    assert!(root_changeset.parents.is_empty());
    let files = scion_files_with_content(&c, &root_cs).await;
    assert_eq!(files[&b"group.txt".to_vec()].0.mode, Mode::File);
    assert_eq!(files[&b"tools/run.sh".to_vec()].0.mode, Mode::Executable);
    let link = &files[&b"link".to_vec()];
    assert_eq!(link.0.mode, Mode::Symlink);
    assert_eq!(link.1, b"README");
    assert!(
        root_changeset
            .delta
            .iter()
            .all(|d| matches!(d.op, DeltaOp::Put { .. }))
    );

    // Empty commits: empty delta, same root tree as the parent.
    for subject in ["empty commit", "empty commit 2"] {
        let g = by_subject(&commits, subject);
        let cs = reader.changeset(&mapped(&c, &g.id).await).await.unwrap();
        let parent = reader
            .changeset(&mapped(&c, &g.parents[0]).await)
            .await
            .unwrap();
        assert!(cs.delta.is_empty(), "{subject}: delta {:?}", cs.delta);
        assert_eq!(cs.root_tree, parent.root_tree, "{subject}");
        assert_eq!(cs.parents.len(), 1);
    }

    // Octopus merge keeps all four parents, in order.
    let oct = by_subject(&commits, "octopus: merge f1, f2, f3");
    assert_eq!(oct.parents.len(), 4);
    let oct_cs = reader.changeset(&mapped(&c, &oct.id).await).await.unwrap();
    let mut want = Vec::new();
    for p in &oct.parents {
        want.push(mapped(&c, p).await);
    }
    assert_eq!(oct_cs.parents, want);

    // Multiple roots: the orphan roots are parentless; one has the empty tree.
    let orphan = by_subject(&commits, "orphan root: other/root.txt");
    let orphan_cs = reader
        .changeset(&mapped(&c, &orphan.id).await)
        .await
        .unwrap();
    assert!(orphan_cs.parents.is_empty());
    assert_eq!(orphan_cs.delta.len(), 1);
    let empty_root = by_subject(&commits, "orphan root with an empty tree");
    let empty_root_cs = reader
        .changeset(&mapped(&c, &empty_root.id).await)
        .await
        .unwrap();
    assert!(empty_root_cs.parents.is_empty());
    assert!(empty_root_cs.delta.is_empty());
    assert_eq!(empty_root_cs.root_tree, empty_tree_id());
    let joined = by_subject(&commits, "merge unrelated histories: orphan and empty-root");
    let joined_cs = reader
        .changeset(&mapped(&c, &joined.id).await)
        .await
        .unwrap();
    assert_eq!(joined_cs.parents.len(), 3);

    // Executable flip: 100755 -> 100644 is a Put with mode File.
    let flip = by_subject(
        &commits,
        "blame: prepend line 0, drop line 10; retarget link; run.sh not executable",
    );
    let flip_cs = reader.changeset(&mapped(&c, &flip.id).await).await.unwrap();
    let run = flip_cs
        .delta
        .iter()
        .find(|d| d.path.as_bytes() == b"tools/run.sh")
        .expect("run.sh in delta");
    assert!(matches!(
        run.op,
        DeltaOp::Put {
            mode: Mode::File,
            ..
        }
    ));

    // Gitlink: omitted from the tree, recorded in extension tag 4 of every commit holding it.
    let gl = by_subject(&commits, "add gitlink vendor/sub");
    let gl_cs = reader.changeset(&mapped(&c, &gl.id).await).await.unwrap();
    assert!(
        gl_cs.delta.is_empty(),
        "gitlink-only commit has an empty delta"
    );
    let meta = reader.meta(&gl_cs.meta).await.unwrap();
    let ext = meta
        .extensions
        .get(META_EXT_SKIPPED_GITLINKS)
        .expect("tag 4");
    let mut expect_ext = vec![1u8, 10];
    expect_ext.extend_from_slice(b"vendor/sub");
    let sha: Vec<u8> = (0..20)
        .map(|i| u8::from_str_radix(&GITLINK_SHA[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    expect_ext.extend_from_slice(&sha);
    assert_eq!(ext, &expect_ext[..]);
    assert_eq!(
        sha_hex(&<[u8; 20]>::try_from(sha.as_slice()).unwrap()),
        GITLINK_SHA
    );
    let before_gl = reader
        .changeset(&mapped(&c, &gl.parents[0]).await)
        .await
        .unwrap();
    let meta = reader.meta(&before_gl.meta).await.unwrap();
    assert!(meta.extensions.get(META_EXT_SKIPPED_GITLINKS).is_none());
    let files = scion_files_with_content(&c, &mapped(&c, &tip.id).await).await;
    assert!(files.keys().all(|p| !p.starts_with(b"vendor")));

    // Re-import into main: nothing new, ref untouched.
    let mem = s.mem.as_ref().unwrap();
    let counts = mem.meta.table_counts();
    let again = scion_import::import(&c, &options(&repo, "main", "main", true))
        .await
        .unwrap();
    assert_eq!(again.imported, 0);
    assert_eq!(again.skipped_existing, again.commits_seen);
    assert_eq!(again.ref_version, 2);
    assert_eq!(again.final_target, rep.final_target);
    assert_eq!(mem.meta.table_counts(), counts);
}

#[tokio::test(flavor = "multi_thread")]
async fn incremental_import_adds_only_new_commits_and_fast_forwards() {
    let s = server().await;
    let c = importer_client(&s).await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = unpack("synthetic-edge", tmp.path());
    let old = git_commits(&repo, "main~3").unwrap();
    let all = git_commits(&repo, "main").unwrap();
    assert!(old.len() < all.len());

    let r1 = scion_import::import(&c, &options(&repo, "main~3", "imp/inc", true))
        .await
        .unwrap();
    assert_eq!(r1.imported, old.len() as u64);
    assert_eq!(r1.ref_version, 1);

    let mem = s.mem.as_ref().unwrap();
    let before = mem.meta.table_counts();
    let r2 = scion_import::import(&c, &options(&repo, "main", "imp/inc", true))
        .await
        .unwrap();
    assert_eq!(r2.commits_seen, all.len() as u64);
    assert_eq!(r2.imported, (all.len() - old.len()) as u64);
    assert_eq!(r2.skipped_existing, old.len() as u64);
    assert_eq!(r2.ref_version, 2);
    let after = mem.meta.table_counts();
    assert_eq!(after.changesets - before.changesets, r2.imported as usize);

    // The ref moved by a fast-forward from the old tip to the new one.
    let log = c
        .refs()
        .get_ref_log(v1::GetRefLogRequest {
            name: "imp/inc".into(),
            from_version: 0,
            limit: 10,
        })
        .await
        .unwrap()
        .into_inner();
    let last = log.entries.iter().find(|e| e.version == 2).expect("v2");
    assert_eq!(
        last.old_target,
        import_support::idb(&r1.final_target.unwrap())
    );
    assert_eq!(
        last.new_target,
        import_support::idb(&r2.final_target.unwrap())
    );
    let ff = c
        .query()
        .is_ancestor(v1::IsAncestorRequest {
            a: idb(&r1.final_target.unwrap()),
            b: idb(&r2.final_target.unwrap()),
        })
        .await
        .unwrap()
        .into_inner()
        .result;
    assert!(ff);
    let f = check_fidelity(&c, &repo, "main").await.unwrap();
    assert!(f.mismatches.is_empty(), "{:?}", f.mismatches);
}

#[tokio::test(flavor = "multi_thread")]
async fn non_fast_forward_target_is_refused() {
    let s = server().await;
    let c = importer_client(&s).await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = unpack("synthetic-edge", tmp.path());
    let r1 = scion_import::import(&c, &options(&repo, "f1", "imp/nff", true))
        .await
        .unwrap();
    let r = scion_import::import(&c, &options(&repo, "orphan", "imp/nff", true)).await;
    assert!(is_code(&r, ErrorCode::NotFastForward), "{r:?}");
    let (target, version) = resolve_ref(&c, "imp/nff").await.unwrap();
    assert_eq!(Some(target), r1.final_target);
    assert_eq!(version, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_git_ref_is_not_found() {
    let s = server().await;
    let c = importer_client(&s).await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = unpack("synthetic-edge", tmp.path());
    let r = scion_import::import(&c, &options(&repo, "no-such-branch", "imp/x", true)).await;
    assert!(is_code(&r, ErrorCode::NotFound), "{r:?}");
    let r = scion_import::import(&c, &options(tmp.path(), "main", "imp/x", true)).await;
    assert!(r.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn importer_without_admin_refs_is_denied() {
    // The default ACL gives service:importer write on / but no admin.refs.
    let s = scion_server::testing::InProcessServer::start(Default::default()).await;
    let c = s
        .client(Some(&scion_import::testing::importer_principal()))
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = unpack("synthetic-edge", tmp.path());
    let r = scion_import::import(&c, &options(&repo, "f1", "imp/denied", true)).await;
    assert!(is_code(&r, ErrorCode::PermissionDenied), "{r:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limited_server_still_imports() {
    // A tight per-principal limit: the importer backs off on RESOURCE_EXHAUSTED.
    let s = server_with_rate(200, 5).await;
    let c = importer_client(&s).await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = unpack("synthetic-edge", tmp.path());
    let rep = scion_import::import(&c, &options(&repo, "main", "imp/rl", true))
        .await
        .unwrap();
    assert_eq!(
        rep.imported,
        git_commits(&repo, "main").unwrap().len() as u64
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tiny_upload_batches_upload_every_blob_once() {
    let s = server().await;
    let c = importer_client(&s).await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = unpack("cfg-if", tmp.path());
    let mut opts = options(&repo, "HEAD", "imp/batch", false);
    opts.batch_bytes = 1;
    let rep = scion_import::import(&c, &opts).await.unwrap();
    // Every distinct blob of the history was uploaded exactly once.
    let mut blobs = std::collections::BTreeSet::new();
    for commit in git_commits(&repo, "HEAD").unwrap() {
        for e in scion_testkit::git::ls_tree(&repo, &commit.id).unwrap() {
            if let Some(content) = e.content {
                blobs.insert(ObjectId::hash_blob(&content));
            }
        }
    }
    assert_eq!(rep.blobs_uploaded, blobs.len() as u64);
}

#[tokio::test(flavor = "multi_thread")]
async fn fixture_file_directory_transitions_modes_and_nested_gitlinks() {
    if !git_available() {
        println!("SKIP: git CLI absent");
        return;
    }
    let commit =
        |files: Vec<(&str, u32, &[u8])>, deletes: Vec<&str>, message: &str| GitOp::Commit {
            branch: "main".into(),
            files: files
                .into_iter()
                .map(|(p, m, c)| (p.to_owned(), m, c.to_vec()))
                .collect(),
            deletes: deletes.into_iter().map(str::to_owned).collect(),
            gitlinks: vec![],
            message: message.into(),
        };
    let fx = GitFixture::build(&[
        GitOp::Commit {
            branch: "main".into(),
            files: vec![
                ("a".into(), 0o100644, b"x\n".to_vec()),
                ("d/keep.txt".into(), 0o100664, b"k\n".to_vec()),
                ("bin/tool".into(), 0o100755, b"#!/bin/sh\n".to_vec()),
                ("ln".into(), 0o120000, b"a".to_vec()),
            ],
            deletes: vec![],
            gitlinks: vec![("only/sub".into(), [0xab; 20])],
            message: "root".into(),
        },
        commit(vec![("a/b", 0o100644, b"y\n")], vec!["a"], "file to dir"),
        commit(vec![("a", 0o100755, b"z\n")], vec!["a/b"], "dir to file"),
        commit(
            vec![("d/keep.txt", 0o100644, b"k\n")],
            vec![],
            "mode normalize",
        ),
        commit(vec![], vec!["bin/tool"], "delete last file of a dir"),
    ])
    .unwrap();
    let s = server().await;
    let c = importer_client(&s).await;
    let repo = fx.path().to_path_buf();
    let r = scion_import::import(&c, &options(&repo, "main", "imp/fx", false)).await;
    assert!(is_code(&r, ErrorCode::InvalidArgument), "{r:?}");
    let rep = scion_import::import(&c, &options(&repo, "main", "imp/fx", true))
        .await
        .unwrap();
    assert_eq!(rep.imported, 5);
    assert_eq!(
        rep.skipped_gitlinks,
        vec![("only/sub".to_owned(), "ab".repeat(20))]
    );
    let f = check_fidelity(&c, &repo, "main").await.unwrap();
    assert!(f.mismatches.is_empty(), "{:?}", f.mismatches);

    let commits = git_commits(&repo, "main").unwrap();
    let reader = ScionReader::new(&c);
    let delta_of = |subject: &str| {
        let id = by_subject(&commits, subject).id.clone();
        let c = &c;
        let reader = &reader;
        async move {
            let cs = reader.changeset(&mapped(c, &id).await).await.unwrap();
            cs.delta
                .iter()
                .map(|d| {
                    let op = match &d.op {
                        DeltaOp::Put { mode, .. } => format!("put{}", *mode as u8),
                        DeltaOp::Delete => "del".to_owned(),
                    };
                    (String::from_utf8_lossy(d.path.as_bytes()).into_owned(), op)
                })
                .collect::<Vec<_>>()
        }
    };
    let s = |a: &str, b: &str| (a.to_owned(), b.to_owned());
    assert_eq!(
        delta_of("root").await,
        vec![
            s("a", "put1"),
            s("bin/tool", "put2"),
            s("d/keep.txt", "put1"),
            s("ln", "put3")
        ]
    );
    assert_eq!(
        delta_of("file to dir").await,
        vec![s("a", "del"), s("a/b", "put1")]
    );
    assert_eq!(
        delta_of("dir to file").await,
        vec![s("a", "put2"), s("a/b", "del")]
    );
    // 100664 -> 100644 with the same content is no Scion change.
    assert_eq!(delta_of("mode normalize").await, vec![]);
    assert_eq!(
        delta_of("delete last file of a dir").await,
        vec![s("bin/tool", "del")]
    );
    // The directory holding only the gitlink does not exist in Scion.
    let tip = mapped(&c, &commits.last().unwrap().id).await;
    let files = scion_files_with_content(&c, &tip).await;
    assert!(files.keys().all(|p| !p.starts_with(b"only")));
    assert!(files.keys().all(|p| !p.starts_with(b"bin")));
}
