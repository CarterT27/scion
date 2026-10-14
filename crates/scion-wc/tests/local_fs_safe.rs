//! SPEC 19.10 path safety and case collisions (SPEC 4.4, 11.3, 17; PLAN 6.17) for P11.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code (PLAN 5.1)

mod local_support;

use local_support::{rp, rpb};
use scion_core::{ErrorCode, Mode, subcode};
use scion_wc::fs_safe::{SafeRoot, case_collisions, check_entry_path, validate_entry_path};
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;

fn names(dir: &std::path::Path) -> Vec<OsString> {
    let mut v: Vec<OsString> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    v.sort();
    v
}

#[test]
fn malicious_entry_paths_are_refused() {
    // These are all refused before any RepoPath is constructed, so this runs without P01.
    for bad in [
        &b".."[..],
        b"a/..",
        b"../etc/passwd",
        b"a/../../b",
        b".scion",
        b".SCION",
        b".Scion/config.toml",
        b"dir/.sCiOn/x",
        b"a\0b",
        b"\0",
        b".",
        b"a/./b",
        b"/abs",
        b"trailing/",
        b"a//b",
        b"",
    ] {
        let err = validate_entry_path(bad).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{bad:?}");
        assert_eq!(err.subcode(), Some(subcode::INVALID_PATH), "{bad:?}");
    }
    check_entry_path(b"fine/.scionrc").unwrap();
}

#[test]
fn valid_entry_paths_are_accepted() {
    require_p01!();
    for ok in [&b"a"[..], b"a/b/c.txt", b"\xff/bytes", b".scionx", b"x/..y"] {
        assert_eq!(validate_entry_path(ok).unwrap().as_bytes(), ok);
    }
}

#[test]
fn writes_through_symlinked_parents_are_refused() {
    require_p01!();
    let wc = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("victim"), b"keep").unwrap();
    std::os::unix::fs::symlink(outside.path(), wc.path().join("evil")).unwrap();
    std::fs::create_dir_all(wc.path().join("a/b")).unwrap();
    std::os::unix::fs::symlink(outside.path(), wc.path().join("a/b/up")).unwrap();
    std::os::unix::fs::symlink("../../..", wc.path().join("a/rel")).unwrap();
    let root = SafeRoot::open(wc.path()).unwrap();

    for p in [
        "evil/x",
        "evil/new/dir/x",
        "a/b/up/x",
        "a/rel/x",
        "evil/victim",
    ] {
        let p = rp(p);
        let err = root
            .write_file_atomic(&p, b"pwned", Mode::File)
            .unwrap_err();
        assert_eq!(err.subcode(), Some(subcode::INVALID_PATH));
        assert!(
            root.write_file_atomic(&p, b"pwned", Mode::Executable)
                .is_err()
        );
        assert!(root.symlink(&p, b"/etc/passwd").is_err());
        assert!(root.read(&p).is_err());
        assert!(root.read_link(&p).is_err());
        assert_eq!(root.stat(&p).unwrap(), None);
        assert!(root.create_dir_all(&p).is_err());
    }
    assert!(root.remove(&rp("evil/victim")).is_err());
    assert_eq!(names(outside.path()), vec![OsString::from("victim")]);
    assert_eq!(
        std::fs::read(outside.path().join("victim")).unwrap(),
        b"keep"
    );
    // The symlinks themselves are untouched.
    assert!(
        wc.path()
            .join("evil")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn write_modes_symlinks_and_removal() {
    require_p01!();
    let wc = tempfile::tempdir().unwrap();
    let root = SafeRoot::open(wc.path()).unwrap();
    root.write_file_atomic(&rp("bin/run"), b"#!/bin/sh\n", Mode::Executable)
        .unwrap();
    root.write_file_atomic(&rp("doc/readme"), b"hi\n", Mode::File)
        .unwrap();
    root.symlink(&rp("doc/link"), b"readme").unwrap();
    root.write_file_atomic(&rp("doc/link2"), b"../bin/run", Mode::Symlink)
        .unwrap();
    let mode = |p: &str| {
        std::fs::symlink_metadata(wc.path().join(p))
            .unwrap()
            .permissions()
            .mode()
    };
    assert_eq!(mode("bin/run") & 0o777, 0o755);
    assert_eq!(mode("doc/readme") & 0o777, 0o644);
    assert_eq!(root.read_link(&rp("doc/link")).unwrap(), b"readme");
    assert_eq!(root.read_link(&rp("doc/link2")).unwrap(), b"../bin/run");
    assert_eq!(root.read(&rp("doc/readme")).unwrap(), b"hi\n");
    let st = root.stat(&rp("bin/run")).unwrap().unwrap();
    assert_eq!(st.entry_mode(), Some(Mode::Executable));
    assert_eq!(
        root.stat(&rp("doc/link")).unwrap().unwrap().entry_mode(),
        Some(Mode::Symlink)
    );
    // Stat agrees with std's lstat.
    let md = std::fs::symlink_metadata(wc.path().join("bin/run")).unwrap();
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(st.inode, md.ino());
        assert_eq!(st.size, md.size());
        assert_eq!(
            st.mtime_ns,
            i128::from(md.mtime()) * 1_000_000_000 + i128::from(md.mtime_nsec())
        );
    }
    // Replacing a symlink with a file and vice versa.
    root.write_file_atomic(&rp("doc/link"), b"now a file", Mode::File)
        .unwrap();
    assert_eq!(root.read(&rp("doc/link")).unwrap(), b"now a file");
    root.symlink(&rp("doc/readme"), b"link").unwrap();
    assert_eq!(root.read_link(&rp("doc/readme")).unwrap(), b"link");

    for p in ["doc/readme", "doc/link", "doc/link2"] {
        root.remove(&rp(p)).unwrap();
    }
    assert!(!wc.path().join("doc").exists());
    root.remove(&rp("bin/run")).unwrap();
    assert!(names(wc.path()).is_empty());
    root.create_dir_all(&rp("x/y/z")).unwrap();
    assert!(wc.path().join("x/y/z").is_dir());
}

#[test]
fn case_collisions_are_deterministic() {
    require_p01!();
    let paths: Vec<_> = [
        "Makefile", "README", "docs/a", "Docs/b", "readme", "src/x", "src/X", "ok",
    ]
    .iter()
    .map(|s| rp(s))
    .collect();
    let got = case_collisions(&paths);
    assert_eq!(
        got,
        vec![
            (rp("Docs/b"), rp("docs/a")),
            (rp("README"), rp("readme")),
            (rp("src/X"), rp("src/x")),
        ]
    );
    // Order of the input does not matter.
    let mut rev = paths.clone();
    rev.reverse();
    assert_eq!(case_collisions(&rev), got);
    // Non-ASCII bytes are not folded.
    assert!(case_collisions(&[rpb(b"\xc3\x84"), rpb(b"\xc3\xa4")]).is_empty());
    assert!(case_collisions(&[rp("same"), rp("same")]).is_empty());
}

#[test]
fn real_case_insensitive_filesystem_collision() {
    require_p01!();
    let wc = tempfile::tempdir().unwrap();
    let root = SafeRoot::open(wc.path()).unwrap();
    if !root.is_case_insensitive() {
        eprintln!("SKIP: temp filesystem is case-sensitive");
        return;
    }
    let entries = [(rp("README"), &b"upper"[..]), (rp("readme"), &b"lower"[..])];
    let paths: Vec<_> = entries.iter().map(|(p, _)| p.clone()).collect();
    let collisions = case_collisions(&paths);
    assert_eq!(collisions, vec![(rp("README"), rp("readme"))]);
    // Checkout policy (PLAN 6.17): colliding entries are reported, not written over each other.
    let skip: std::collections::BTreeSet<_> = collisions.iter().map(|(_, b)| b.clone()).collect();
    for (p, data) in &entries {
        if !skip.contains(p) {
            root.write_file_atomic(p, data, Mode::File).unwrap();
        }
    }
    assert_eq!(root.read(&rp("README")).unwrap(), b"upper");
    // The filesystem really conflates the two names.
    assert!(root.stat(&rp("readme")).unwrap().is_some());
    assert_eq!(root.read(&rp("readme")).unwrap(), b"upper");
}
