//! SPEC 19.10 commit and amend against the real in-process server (P28): only missing blobs
//! are uploaded, the delta equals the reference model's file diff, amend preserves the change
//! id, commits cannot include out-of-scope or inaccessible paths, and editing a redaction
//! placeholder commits new content normally (SPEC 13.7).

mod commit_support;

use commit_support::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use scion_core::{ErrorCode, Mode, ObjectId};
use scion_object::{DeltaOp, RepoPath};
use scion_refmodel::{MDelta, MFile, MTree};
use scion_wc::testing::{
    Snapshot, amend_preserves_change_id, checkout_at, commit_uploads_only_missing, is_path_refusal,
    remove_files, seed_files, wc_snapshot, write_files,
};
use scion_wc::{WcOptions, WorkingCopy};

fn mtree(s: &Snapshot) -> MTree {
    s.iter()
        .map(|(p, (m, c))| {
            (
                p.as_bytes().to_vec(),
                MFile {
                    mode: *m as u8,
                    content: c.clone(),
                },
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_uploads_only_missing_blobs() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let report = commit_uploads_only_missing(&s, &main_ref()).await.unwrap();
    assert_eq!(report.uploaded, 4, "{report:?}");
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_delta_equals_refmodel_file_diff() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let r = main_ref();
    let mut seed: Vec<(String, Mode, Vec<u8>)> = Vec::new();
    for i in 0..12 {
        seed.push((
            format!("d{}/f{i}.txt", i % 3),
            Mode::File,
            format!("file {i}\n").into_bytes(),
        ));
    }
    seed.push(("run.sh".into(), Mode::Executable, b"#!/bin/sh\n".to_vec()));
    seed.push(("ln".into(), Mode::Symlink, b"d0/f0.txt".to_vec()));
    let specs: Vec<(&str, Mode, &[u8])> = seed
        .iter()
        .map(|(p, m, c)| (p.as_str(), *m, c.as_slice()))
        .collect();
    seed_files(&s, &r, &specs, &[]).await.unwrap();
    let objects_before = srv.mem.as_ref().unwrap().meta.all_objects().len();

    for seed_no in 0..4u64 {
        let mut rng = StdRng::seed_from_u64(seed_no);
        let mut wc = checkout_at(&s, r.as_str(), None).await.unwrap();
        let before = wc_snapshot(&wc).unwrap();
        let paths: Vec<RepoPath> = before.keys().cloned().collect();
        for (k, p) in paths.iter().enumerate() {
            let ps = String::from_utf8(p.as_bytes().to_vec()).unwrap();
            match rng.random_range(0..6) {
                0 => remove_files(&wc, &[&ps]).unwrap(),
                1 => {
                    let c = format!("edited {seed_no} {k}\n");
                    write_files(&wc, &[(&ps, Mode::File, c.as_bytes())]).unwrap();
                }
                2 => {
                    let (m, c) = &before[p];
                    if *m != Mode::Symlink {
                        let nm = if *m == Mode::File {
                            Mode::Executable
                        } else {
                            Mode::File
                        };
                        write_files(&wc, &[(&ps, nm, c)]).unwrap();
                    }
                }
                _ => {}
            }
        }
        let added = format!("new/added{seed_no}.txt");
        write_files(&wc, &[(&added, Mode::File, b"added\n")]).unwrap();
        write_files(&wc, &[("new/link", Mode::Symlink, b"../ln")]).unwrap();
        let after = wc_snapshot(&wc).unwrap();
        let expected = scion_refmodel::file_diff(&mtree(&before), &mtree(&after));
        let report = wc
            .commit_with_report(b"random edits\n", b"Alice <a@x>", None)
            .await
            .unwrap();
        // Same paths, same order, same operations (ids recomputed from the model's bytes).
        assert_eq!(report.delta.len(), expected.len());
        for (got, want) in report.delta.iter().zip(&expected) {
            match (want, &got.op) {
                (MDelta::Put(p, f), DeltaOp::Put { mode, id, size }) => {
                    assert_eq!(p.as_slice(), got.path.as_bytes());
                    assert_eq!(f.mode, *mode as u8);
                    assert_eq!(*id, ObjectId::hash_blob(&f.content));
                    assert_eq!(*size, f.content.len() as u64);
                }
                (MDelta::Delete(p), DeltaOp::Delete) => {
                    assert_eq!(p.as_slice(), got.path.as_bytes())
                }
                other => panic!("delta mismatch: {other:?}"),
            }
        }
        assert!(wc.status().await.unwrap().is_clean());
    }
    assert!(srv.mem.as_ref().unwrap().meta.all_objects().len() > objects_before);
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn amend_keeps_change_id() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    amend_preserves_change_id(&s, &main_ref()).await.unwrap();
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_to_commit_and_path_restriction() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let r = main_ref();
    seed_files(
        &s,
        &r,
        &[("a/x", Mode::File, b"x\n"), ("b/y", Mode::File, b"y\n")],
        &[],
    )
    .await
    .unwrap();
    let mut wc = checkout_at(&s, r.as_str(), None).await.unwrap();
    let e = wc.commit(b"m", b"A", None).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::EmptyChange);
    assert!(wc.state().pending.is_none());

    write_files(
        &wc,
        &[("a/x", Mode::File, b"x2\n"), ("b/y", Mode::File, b"y2\n")],
    )
    .unwrap();
    let report = wc
        .commit_with_report(b"only a\n", b"A", Some(&[rp("a")]))
        .await
        .unwrap();
    assert_eq!(report.delta.len(), 1);
    assert_eq!(report.delta[0].path, rp("a/x"));
    let st = wc.status().await.unwrap();
    assert_eq!(st.modified, vec![rp("b/y")]);
    // A path that matches nothing in the selection is EMPTY_CHANGE.
    let e = wc.commit(b"m", b"A", Some(&[rp("a")])).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::EmptyChange);
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn commits_exclude_out_of_scope_paths() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let r = main_ref();
    seed_files(
        &s,
        &r,
        &[("in/x", Mode::File, b"x\n"), ("out/y", Mode::File, b"y\n")],
        &[],
    )
    .await
    .unwrap();
    let mut wc = WorkingCopy::checkout(
        s.client.clone(),
        &s.fresh_dir("sparse"),
        r.as_str(),
        vec![rp("in")],
        WcOptions::default(),
    )
    .await
    .unwrap();
    // A file created outside the scope is never committed.
    write_files(
        &wc,
        &[("out/z", Mode::File, b"z\n"), ("in/x", Mode::File, b"x2\n")],
    )
    .unwrap();
    let e = wc.commit(b"m", b"A", Some(&[rp("out")])).await.unwrap_err();
    assert!(is_path_refusal(&e), "{e}");
    assert_eq!(e.subcode(), Some(WorkingCopy::SUBCODE_OUT_OF_SCOPE));
    let report = wc.commit_with_report(b"m\n", b"A", None).await.unwrap();
    let paths: Vec<RepoPath> = report.delta.iter().map(|d| d.path.clone()).collect();
    assert_eq!(paths, vec![rp("in/x")]);
    // The out-of-scope file stays on the server as it was.
    let full = scion_wc::testing::checkout_snapshot(&s, &report.changeset.to_text(), &[])
        .await
        .unwrap();
    assert_eq!(full[&rp("out/y")].1, b"y\n");
    assert!(!full.contains_key(&rp("out/z")));
    validate_server(&srv).await;
}

const RESTRICTED_ACL: &str = r#"version = 1

[groups]
eng = ["github:alice", "group:admins"]
admins = ["github:admin"]

[[paths]]
prefix = "/"
read = ["group:eng"]
write = ["group:eng"]

[[paths]]
prefix = "/secret"
read = ["group:admins"]
write = ["group:admins"]

[refs."*"]
update = ["group:eng"]
force = ["group:admins"]

[admin]
redact = ["group:admins"]
purge = ["group:admins"]
refs = ["group:admins"]
"#;

#[tokio::test(flavor = "multi_thread")]
async fn commits_exclude_inaccessible_paths() {
    let srv = server_with_acl(RESTRICTED_ACL).await;
    let r = main_ref();
    let admin_s = scenario(&srv, &admin()).await;
    seed_files(
        &admin_s,
        &r,
        &[
            ("secret/k", Mode::File, b"key\n"),
            ("pub/p", Mode::File, b"p\n"),
        ],
        &[],
    )
    .await
    .unwrap();
    let s = scenario(&srv, &alice()).await;
    let mut wc = checkout_at(&s, r.as_str(), None).await.unwrap();
    assert_eq!(wc.state().inaccessible, vec![rp("secret")]);
    write_files(
        &wc,
        &[
            ("secret/mine", Mode::File, b"m\n"),
            ("pub/p", Mode::File, b"p2\n"),
        ],
    )
    .unwrap();
    let e = wc
        .commit(b"m", b"A", Some(&[rp("secret/mine")]))
        .await
        .unwrap_err();
    assert!(is_path_refusal(&e), "{e}");
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    let report = wc.commit_with_report(b"m\n", b"A", None).await.unwrap();
    let paths: Vec<RepoPath> = report.delta.iter().map(|d| d.path.clone()).collect();
    assert_eq!(paths, vec![rp("pub/p")]);
    assert!(wc.status().await.unwrap().is_clean());
    // The restricted file is unchanged on the server.
    let full = scion_wc::testing::checkout_snapshot(&admin_s, &report.changeset.to_text(), &[])
        .await
        .unwrap();
    assert_eq!(full[&rp("secret/k")].1, b"key\n");
    assert!(!full.contains_key(&rp("secret/mine")));
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn editing_a_redacted_placeholder_commits_new_content() {
    let srv = server().await;
    let r = main_ref();
    let s = scenario(&srv, &alice()).await;
    let secret = b"leaked secret\n".to_vec();
    seed_files(
        &s,
        &r,
        &[
            ("cfg/secret.txt", Mode::File, &secret),
            ("cfg/other", Mode::File, b"o\n"),
        ],
        &[],
    )
    .await
    .unwrap();
    let blob = ObjectId::hash_blob(&secret);
    let admin_client = srv.client(Some(&admin())).await;
    admin_client
        .admin()
        .redact(scion_proto::v1::RedactRequest {
            object_ids: vec![blob.as_bytes().to_vec()],
            reason: "test".into(),
            purge_after_seconds: None,
        })
        .await
        .unwrap();
    let mut wc = checkout_at(&s, r.as_str(), None).await.unwrap();
    let placeholder = scion_wc::placeholder_bytes(&blob);
    assert_eq!(
        wc_snapshot(&wc).unwrap()[&rp("cfg/secret.txt")].1,
        placeholder
    );
    // An unmodified placeholder is clean: nothing to commit.
    assert_eq!(
        wc.commit(b"m", b"A", None).await.unwrap_err().code,
        ErrorCode::EmptyChange
    );
    let mut edited = placeholder.clone();
    edited.extend_from_slice(b"rotated credentials\n");
    write_files(&wc, &[("cfg/secret.txt", Mode::File, &edited)]).unwrap();
    let report = wc
        .commit_with_report(b"replace\n", b"A", None)
        .await
        .unwrap();
    assert_eq!(report.uploaded, 1);
    assert_eq!(report.delta.len(), 1);
    match &report.delta[0].op {
        DeltaOp::Put { id, .. } => assert_eq!(*id, ObjectId::hash_blob(&edited)),
        other => panic!("{other:?}"),
    }
    assert!(wc.status().await.unwrap().is_clean());
    let fresh = scion_wc::testing::checkout_snapshot(&s, &report.changeset.to_text(), &[])
        .await
        .unwrap();
    assert_eq!(fresh[&rp("cfg/secret.txt")].1, edited);
    validate_server(&srv).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn amend_of_root_is_refused_and_amend_message_only() {
    let srv = server().await;
    let s = scenario(&srv, &alice()).await;
    let mut wc = checkout_at(&s, "main", None).await.unwrap();
    // main@1 is the parentless root changeset.
    let e = wc.amend(Some(b"x")).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    write_files(&wc, &[("f", Mode::File, b"1\n")]).unwrap();
    let c = wc.commit_with_report(b"one\n", b"A", None).await.unwrap();
    let a = wc.amend_with_report(Some(b"two\n")).await.unwrap();
    assert_eq!(a.change_id, c.change_id);
    assert_eq!(a.delta, c.delta);
    assert_ne!(a.changeset, c.changeset);
    validate_server(&srv).await;
}
