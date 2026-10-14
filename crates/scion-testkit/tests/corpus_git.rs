//! SPEC 19.0 Corpus / 19.14: vendored git histories exist, are licensed and documented, fit
//! the size limit, and unpack with gix alone (no git CLI). The GitFixture test needs the git
//! CLI and skips when it is absent.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use scion_testkit::git::{
    GitCommitInfo, GitFixture, GitOp, MAX_BUNDLE_BYTES, bundle_header, bundles, commits,
    corpus_dir, git_available, ls_tree, unpack,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

struct Real {
    name: &'static str,
    url: &'static str,
    branch: &'static str,
    tip: &'static str,
    commits: usize,
}

const REAL: [Real; 2] = [
    Real {
        name: "cfg-if",
        url: "https://github.com/rust-lang/cfg-if",
        branch: "refs/heads/main",
        tip: "14432a28e180ec717933714aed7828fc7ed2fd29",
        commits: 137,
    },
    Real {
        name: "itoa",
        url: "https://github.com/dtolnay/itoa",
        branch: "refs/heads/master",
        tip: "1577ed901354d0d7448ac162328f9dbf5183124c",
        commits: 330,
    },
];

fn bundle(name: &str) -> PathBuf {
    let path = corpus_dir().join(format!("{name}.bundle"));
    assert!(bundles().contains(&path), "{} is listed", path.display());
    path
}

fn message(c: &GitCommitInfo) -> String {
    let raw = String::from_utf8_lossy(&c.raw).to_string();
    raw.split_once("\n\n")
        .map(|(_, m)| m.to_string())
        .unwrap_or_default()
}

fn by_id(cs: &[GitCommitInfo]) -> BTreeMap<&str, &GitCommitInfo> {
    cs.iter().map(|c| (c.id.as_str(), c)).collect()
}

#[test]
fn bundles_exist_within_size_limit() {
    let all = bundles();
    assert!(all.len() >= 3, "{all:?}");
    for b in &all {
        let size = std::fs::metadata(b).map(|m| m.len()).unwrap_or(u64::MAX);
        assert!(size <= MAX_BUNDLE_BYTES, "{} is {size} bytes", b.display());
        assert!(size > 0);
        let h = bundle_header(b).unwrap();
        assert!(h.prerequisites.is_empty(), "full history, no prerequisites");
        assert!(!h.refs.is_empty());
    }
    for r in &REAL {
        let h = bundle_header(&bundle(r.name)).unwrap();
        assert!(
            h.refs.iter().any(|(n, id)| n == r.branch && id == r.tip),
            "{}: bundled tip matches README",
            r.name
        );
    }
}

#[test]
fn readme_and_licenses_are_present() {
    let dir = corpus_dir();
    let readme = std::fs::read_to_string(dir.join("README.md")).unwrap();
    for r in &REAL {
        assert!(readme.contains(r.url), "README names {}", r.url);
        assert!(
            readme.contains(r.tip),
            "README records the {} commit",
            r.name
        );
        assert!(readme.contains(&format!("LICENSE-{}", r.name)));
        let lic = std::fs::read_to_string(dir.join(format!("LICENSE-{}", r.name))).unwrap();
        assert!(lic.contains(r.url), "license file records the source URL");
        assert!(lic.contains(r.tip), "license file records the commit");
        assert!(lic.contains("MIT OR Apache-2.0"));
        assert!(lic.contains("Permission is hereby granted"), "MIT text");
        assert!(lic.contains("Apache License"), "Apache text");
    }
    assert!(readme.contains("synthetic-edge.bundle"));
    assert!(dir.join("build-synthetic.sh").exists());
}

#[test]
fn real_histories_unpack_with_gix() {
    let tmp = tempfile::tempdir().unwrap();
    for r in &REAL {
        let repo = unpack(&bundle(r.name), tmp.path()).unwrap();
        let cs = commits(&repo).unwrap();
        assert_eq!(cs.len(), r.commits, "{}: commit count", r.name);
        let ids = by_id(&cs);
        // Parents first, every parent present.
        let mut seen = BTreeSet::new();
        for c in &cs {
            for p in &c.parents {
                assert!(seen.contains(p.as_str()), "{}: parent order", r.name);
            }
            seen.insert(c.id.as_str());
        }
        assert!(
            cs.iter().filter(|c| c.parents.len() >= 2).count() >= 5,
            "has merges"
        );
        assert!(ids.contains_key(r.tip));
        let files = ls_tree(&repo, r.tip).unwrap();
        let map: BTreeMap<&[u8], _> = files.iter().map(|e| (e.path.as_slice(), e)).collect();
        let cargo = map.get(b"Cargo.toml".as_slice()).expect("Cargo.toml");
        assert_eq!(cargo.mode, 0o100644);
        let text = String::from_utf8_lossy(cargo.content.as_deref().unwrap_or_default());
        assert!(text.contains("MIT OR Apache-2.0"));
        assert!(map.contains_key(b"LICENSE-MIT".as_slice()));
        assert!(files.iter().any(|e| e.path.starts_with(b"src/")));
        // HEAD points at the default branch.
        let head = std::fs::read_to_string(repo.join("HEAD")).unwrap();
        assert_eq!(head.trim(), format!("ref: {}", r.branch));
        // Every commit's tree is readable.
        for c in cs.iter().step_by(17) {
            ls_tree(&repo, &c.id).unwrap();
        }
    }
}

fn synthetic(tmp: &Path) -> (PathBuf, Vec<GitCommitInfo>) {
    let repo = unpack(&bundle("synthetic-edge"), tmp).unwrap();
    let cs = commits(&repo).unwrap();
    (repo, cs)
}

fn find<'a>(cs: &'a [GitCommitInfo], subject: &str) -> &'a GitCommitInfo {
    cs.iter()
        .find(|c| message(c).starts_with(subject))
        .unwrap_or_else(|| panic!("no commit {subject:?}"))
}

#[test]
fn synthetic_history_has_every_edge_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, cs) = synthetic(tmp.path());
    assert_eq!(cs.len(), 17);
    let ids = by_id(&cs);

    let roots: Vec<_> = cs.iter().filter(|c| c.parents.is_empty()).collect();
    assert_eq!(roots.len(), 3, "multiple roots");
    let octopus = find(&cs, "octopus: merge f1, f2, f3");
    assert_eq!(octopus.parents.len(), 4);
    let unrelated = find(&cs, "merge unrelated histories");
    assert_eq!(unrelated.parents.len(), 3);

    let empties: Vec<_> = cs
        .iter()
        .filter(|c| c.parents.len() == 1 && ids[c.parents[0].as_str()].tree == c.tree)
        .collect();
    assert_eq!(empties.len(), 2, "two empty commits");
    let empty_root = find(&cs, "orphan root with an empty tree");
    assert!(ls_tree(&repo, &empty_root.id).unwrap().is_empty());

    let root = find(&cs, "root: initial files");
    let files = ls_tree(&repo, &root.id).unwrap();
    let mode = |path: &str, fs: &[scion_testkit::git::GitEntry]| {
        fs.iter()
            .find(|e| e.path == path.as_bytes())
            .map(|e| e.mode)
    };
    assert_eq!(
        mode("group.txt", &files),
        Some(0o100664),
        "mode 100664 preserved"
    );
    assert_eq!(mode("tools/run.sh", &files), Some(0o100755));
    assert_eq!(mode("link", &files), Some(0o120000));
    let link = files.iter().find(|e| e.path == b"link").unwrap();
    assert_eq!(
        link.content.as_deref(),
        Some(b"README".as_slice()),
        "symlink target"
    );

    let gitlink = find(&cs, "add gitlink vendor/sub");
    let files = ls_tree(&repo, &gitlink.id).unwrap();
    let sub = files.iter().find(|e| e.path == b"vendor/sub").unwrap();
    assert_eq!(sub.mode, 0o160000);
    assert_eq!(sub.oid, "0123456789abcdef0123456789abcdef01234567");
    assert!(sub.content.is_none());
    let retarget = find(&cs, "blame: prepend line 0");
    let files = ls_tree(&repo, &retarget.id).unwrap();
    let link = files.iter().find(|e| e.path == b"link").unwrap();
    assert_eq!(
        link.content.as_deref(),
        Some(b"blame.txt".as_slice()),
        "retarget"
    );
    assert_eq!(
        mode("tools/run.sh", &files),
        Some(0o100644),
        "executable flip"
    );

    // The merge keeps the first parent's README and takes the side's new file.
    let merge = find(&cs, "merge side keeping");
    assert_eq!(merge.parents.len(), 2);
    let readme = |id: &str| {
        ls_tree(&repo, id)
            .unwrap()
            .into_iter()
            .find(|e| e.path == b"README")
            .and_then(|e| e.content)
    };
    assert_eq!(readme(&merge.id), readme(&merge.parents[0]));
    assert_ne!(readme(&merge.id), readme(&merge.parents[1]));
    assert!(
        ls_tree(&repo, &merge.id)
            .unwrap()
            .iter()
            .any(|e| e.path == b"side.txt")
    );

    // Rename-free line edits of blame.txt along main.
    let blame_versions: BTreeSet<Vec<u8>> = cs
        .iter()
        .filter_map(|c| {
            ls_tree(&repo, &c.id)
                .unwrap()
                .into_iter()
                .find(|e| e.path == b"blame.txt")
                .and_then(|e| e.content)
        })
        .collect();
    assert!(
        blame_versions.len() >= 5,
        "{} blame.txt versions",
        blame_versions.len()
    );

    // Author and committer identities and zones are preserved in the raw commit.
    let raw = String::from_utf8_lossy(&root.raw).to_string();
    assert!(raw.contains("author Scion Fixture Author <author@example.com>"));
    assert!(raw.contains("+0130") && raw.contains("-0700"));
    let head = std::fs::read_to_string(repo.join("HEAD")).unwrap();
    assert_eq!(head.trim(), "ref: refs/heads/main");
}

#[test]
fn unpack_rejects_non_bundles() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = tmp.path().join("fake.bundle");
    std::fs::write(&fake, b"not a bundle\n\n").unwrap();
    assert!(unpack(&fake, tmp.path()).is_err());
    let thin = tmp.path().join("thin.bundle");
    std::fs::write(
        &thin,
        b"# v2 git bundle\n-0123456789abcdef0123456789abcdef01234567 base\n\
          0123456789abcdef0123456789abcdef01234567 refs/heads/main\n\n",
    )
    .unwrap();
    assert!(unpack(&thin, tmp.path()).is_err());
}

#[test]
fn git_fixture_builds_scripts() {
    if !git_available() {
        println!("SKIP: git CLI not available");
        return;
    }
    let c = |branch: &str, files: Vec<(&str, u32, &[u8])>, message: &str| GitOp::Commit {
        branch: branch.to_string(),
        files: files
            .into_iter()
            .map(|(p, m, b)| (p.to_string(), m, b.to_vec()))
            .collect(),
        deletes: Vec::new(),
        gitlinks: Vec::new(),
        message: message.to_string(),
    };
    let script = vec![
        c(
            "main",
            vec![
                ("a.txt", 0o100644, b"a\n"),
                ("bin/run", 0o100755, b"#!/bin/sh\n"),
                ("g.txt", 0o100664, b"group\n"),
                ("ln", 0o120000, b"a.txt"),
            ],
            "root",
        ),
        c("f1", vec![("f1.txt", 0o100644, b"1\n")], "f1"),
        c("f2", vec![("f2.txt", 0o100644, b"2\n")], "f2"),
        c("main", vec![("a.txt", 0o100644, b"a2\n")], "main edit"),
        GitOp::Merge {
            into: "main".into(),
            from: vec!["f1".into(), "f2".into()],
            message: "octopus".into(),
        },
        GitOp::OrphanRoot {
            branch: "other".into(),
        },
        c("other", vec![("o.txt", 0o100644, b"o\n")], "second root"),
        GitOp::Merge {
            into: "main".into(),
            from: vec!["other".into()],
            message: "join".into(),
        },
        GitOp::EmptyCommit {
            branch: "main".into(),
            message: "empty".into(),
        },
        GitOp::Commit {
            branch: "main".into(),
            files: Vec::new(),
            deletes: vec!["bin".into()],
            gitlinks: vec![("vendor/sub".into(), [0xab; 20])],
            message: "gitlink".into(),
        },
    ];
    let fx = GitFixture::build(&script).unwrap();
    let tip = fx.rev_parse("main").unwrap();
    let cs = commits(fx.path()).unwrap();
    assert_eq!(cs.len(), 9);
    assert_eq!(cs.iter().filter(|c| c.parents.is_empty()).count(), 2);
    let octopus = find(&cs, "octopus");
    assert_eq!(octopus.parents.len(), 3);
    let files = ls_tree(fx.path(), &octopus.id).unwrap();
    let names: BTreeSet<&[u8]> = files.iter().map(|e| e.path.as_slice()).collect();
    for p in ["a.txt", "f1.txt", "f2.txt", "g.txt", "ln", "bin/run"] {
        assert!(names.contains(p.as_bytes()), "{p} in octopus tree");
    }
    let a = files.iter().find(|e| e.path == b"a.txt").unwrap();
    assert_eq!(a.content.as_deref(), Some(b"a2\n".as_slice()));
    let g = files.iter().find(|e| e.path == b"g.txt").unwrap();
    assert_eq!(g.mode, 0o100664);
    let empty = find(&cs, "empty");
    assert_eq!(by_id(&cs)[empty.parents[0].as_str()].tree, empty.tree);
    let last = ls_tree(fx.path(), &tip).unwrap();
    assert!(last.iter().all(|e| !e.path.starts_with(b"bin/")));
    let sub = last.iter().find(|e| e.path == b"vendor/sub").unwrap();
    assert_eq!(sub.mode, 0o160000);
    assert_eq!(sub.oid, "ab".repeat(20));
    assert!(last.iter().any(|e| e.path == b"o.txt"));

    // Round trip through a bundle and the gix-only unpacker.
    let tmp = tempfile::tempdir().unwrap();
    let b = tmp.path().join("fixture.bundle");
    fx.bundle(&b).unwrap();
    let repo = unpack(&b, tmp.path()).unwrap();
    let again = commits(&repo).unwrap();
    assert_eq!(again, cs);

    // Determinism: the same script yields the same commit ids.
    let fx2 = GitFixture::build(&script).unwrap();
    assert_eq!(fx2.rev_parse("main").unwrap(), tip);
}
