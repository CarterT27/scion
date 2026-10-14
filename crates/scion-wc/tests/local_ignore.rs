//! SPEC 11.4 / 19.10: `.gitignore` semantics match `git check-ignore` (P11).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code (PLAN 5.1)

mod local_support;

use local_support::have_git;
use scion_wc::ignore::IgnoreMatcher;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Root `.gitignore`: 37 patterns (plus a comment and a blank line).
const ROOT: &str = "# comment line
*.log
!important.log
*.tmp
/root_only.txt
build/
dist
doc/*.txt
doc/**/*.pdf
**/cache
a/**/z
*.[oa]
?.q
\\#hash
\\!bang
target/
!target/keep
out/*
!out/keep.txt
foo/bar
/anchored/dir/
**/deep/**/leaf
*~
.DS_Store
node_modules/

*.py[cod]
logs/**
!logs/.gitkeep
trailing\\
spaced name
[Bb]in/
**/*.bak
x/y/
lit[*]star
abc/**
!abc/def/
trail-ws
";

/// `sub/.gitignore`: 6 patterns.
const SUB: &str = "!*.log
local.cfg
/only_here
*.gen
!keep.gen
nested/
";

/// `sub/deeper/.gitignore`: 2 patterns.
const DEEPER: &str = "*.txt\n!special.txt\n";

/// Files created on disk (and queried as files).
const FILES: &[&str] = &[
    "app.log",
    "important.log",
    "x/important.log",
    "x/y2/z.log",
    "a.tmp",
    "root_only.txt",
    "x/root_only.txt",
    "build/out.bin",
    "src/build/x",
    "dist",
    "dist2/dist/f",
    "doc/a.txt",
    "doc/sub/a.txt",
    "doc/b.pdf",
    "doc/sub/b.pdf",
    "doc/x/y/c.pdf",
    "cache/f",
    "src/cache/f",
    "a/z",
    "a/b/z",
    "a/b/c/z",
    "m.o",
    "m.a",
    "m.c",
    "x.q",
    "xy.q",
    "#hash",
    "!bang",
    "target/x",
    "target/keep",
    "out/a",
    "out/keep.txt",
    "foo/bar",
    "x/foo/bar",
    "anchored/dir/f",
    "x/anchored/dir/f",
    "p/deep/q/leaf",
    "deep/leaf",
    "file~",
    ".DS_Store",
    "node_modules/pkg/index.js",
    "m.pyc",
    "m.pyo",
    "m.py",
    "logs/a",
    "logs/.gitkeep",
    "trailing ",
    "trailing",
    "spaced name",
    "bin/x",
    "Bin/y",
    "BIN/z",
    "q.bak",
    "d/q.bak",
    "x/y/f",
    "x/yy/f",
    "lit*star",
    "litastar",
    "abc/def/g",
    "abc/h",
    "trail-ws",
    "sub/a.log",
    "sub/local.cfg",
    "sub/x/local.cfg",
    "sub/only_here",
    "sub/x/only_here",
    "only_here",
    "sub/a.gen",
    "sub/keep.gen",
    "sub/nested/f",
    "nested/f",
    "sub/deeper/a.txt",
    "sub/deeper/special.txt",
    "sub/deeper/x/b.txt",
    "sub/deeper/x/special.txt",
    "README.md",
    "src/main.rs",
    "APP.LOG",
];

/// Directories queried as directories.
const DIRS: &[&str] = &[
    "build",
    "dist2",
    "target",
    "out",
    "logs",
    "abc",
    "abc/def",
    "sub/nested",
    "x/y",
    "bin",
    "BIN",
    "cache",
    "doc",
    "src",
    "node_modules",
];

/// Expected results (git semantics), independent of git being installed.
const TABLE: &[(&str, bool, bool)] = &[
    ("app.log", false, true),
    ("important.log", false, false),
    ("x/important.log", false, false),
    ("x/y2/z.log", false, true),
    ("root_only.txt", false, true),
    ("x/root_only.txt", false, false),
    ("build/out.bin", false, true),
    ("src/build/x", false, true),
    ("build", true, true),
    ("dist", false, true),
    ("dist2/dist/f", false, true),
    ("doc/a.txt", false, true),
    ("doc/sub/a.txt", false, false),
    ("doc/b.pdf", false, true),
    ("doc/x/y/c.pdf", false, true),
    ("src/cache/f", false, true),
    ("a/z", false, true),
    ("a/b/c/z", false, true),
    ("m.o", false, true),
    ("m.c", false, false),
    ("x.q", false, true),
    ("xy.q", false, false),
    ("#hash", false, true),
    ("!bang", false, true),
    ("target/keep", false, true),
    ("out/a", false, true),
    ("out/keep.txt", false, false),
    ("out", true, false),
    ("foo/bar", false, true),
    ("x/foo/bar", false, false),
    ("anchored/dir/f", false, true),
    ("x/anchored/dir/f", false, false),
    ("p/deep/q/leaf", false, true),
    ("deep/leaf", false, true),
    ("m.pyc", false, true),
    ("m.py", false, false),
    ("logs/a", false, true),
    ("logs/.gitkeep", false, false),
    ("logs", true, false),
    ("Bin/y", false, true),
    ("BIN/z", false, false),
    ("d/q.bak", false, true),
    ("x/y/f", false, true),
    ("x/yy/f", false, false),
    ("lit*star", false, true),
    ("litastar", false, false),
    ("abc/def/g", false, true),
    ("sub/a.log", false, false),
    ("sub/x/local.cfg", false, true),
    ("sub/only_here", false, true),
    ("sub/x/only_here", false, false),
    ("only_here", false, false),
    ("sub/keep.gen", false, false),
    ("sub/nested/f", false, true),
    ("nested/f", false, false),
    ("sub/deeper/a.txt", false, true),
    ("sub/deeper/special.txt", false, false),
    ("sub/deeper/x/b.txt", false, true),
    ("README.md", false, false),
    ("APP.LOG", false, false),
    ("trail-ws", false, true),
];

fn matcher_in_memory() -> IgnoreMatcher {
    IgnoreMatcher::from_files([
        (&b""[..], ROOT.as_bytes()),
        (b"sub", SUB.as_bytes()),
        (b"sub/deeper", DEEPER.as_bytes()),
    ])
}

fn build_tree(root: &Path) {
    std::fs::write(root.join(".gitignore"), ROOT).unwrap();
    std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
    std::fs::write(root.join("sub/.gitignore"), SUB).unwrap();
    std::fs::write(root.join("sub/deeper/.gitignore"), DEEPER).unwrap();
    for f in FILES {
        let p = root.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"x").unwrap();
    }
    for d in DIRS {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
}

fn pattern_count() -> usize {
    [ROOT, SUB, DEEPER]
        .iter()
        .flat_map(|t| t.lines())
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .count()
}

#[test]
fn pure_table_of_expected_results() {
    assert!(pattern_count() >= 40);
    let m = matcher_in_memory();
    let mut wrong = Vec::new();
    for (p, is_dir, expected) in TABLE {
        if m.is_ignored_bytes(p.as_bytes(), *is_dir) != *expected {
            wrong.push((*p, *is_dir, *expected));
        }
    }
    assert!(
        wrong.is_empty(),
        "mismatches (path, is_dir, expected): {wrong:?}"
    );
}

#[test]
fn loaded_from_disk_equals_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    build_tree(dir.path());
    let disk = IgnoreMatcher::load(dir.path()).unwrap();
    let mem = matcher_in_memory();
    for f in FILES {
        assert_eq!(
            disk.is_ignored_bytes(f.as_bytes(), false),
            mem.is_ignored_bytes(f.as_bytes(), false),
            "{f}"
        );
    }
    for d in DIRS {
        assert_eq!(
            disk.is_ignored_bytes(d.as_bytes(), true),
            mem.is_ignored_bytes(d.as_bytes(), true),
            "{d}"
        );
    }
}

#[test]
fn matches_git_check_ignore() {
    if !have_git() {
        eprintln!("SKIP: git CLI not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let git = |args: &[&str]| {
        Command::new("git")
            .args(["-c", "core.ignorecase=false"])
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", root)
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-q"]).status.success());
    build_tree(root);
    let queries: Vec<(&str, bool)> = FILES
        .iter()
        .map(|f| (*f, false))
        .chain(DIRS.iter().map(|d| (*d, true)))
        .collect();
    let mut child = Command::new("git")
        .args([
            "-c",
            "core.ignorecase=false",
            "check-ignore",
            "--no-index",
            "--stdin",
            "-z",
        ])
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for (q, _) in &queries {
            stdin.write_all(q.as_bytes()).unwrap();
            stdin.write_all(b"\0").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        matches!(out.status.code(), Some(0) | Some(1)),
        "git check-ignore failed: {out:?}"
    );
    let ignored: std::collections::BTreeSet<&[u8]> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .collect();
    let m = IgnoreMatcher::load(root).unwrap();
    let mut wrong = Vec::new();
    for (q, is_dir) in &queries {
        let git_says = ignored.contains(q.as_bytes());
        if m.is_ignored_bytes(q.as_bytes(), *is_dir) != git_says {
            wrong.push((*q, *is_dir, git_says));
        }
    }
    assert!(
        wrong.is_empty(),
        "disagreements with git (path, is_dir, git): {wrong:?}"
    );
    // The pure table agrees with git too.
    for (p, _, expected) in TABLE {
        assert_eq!(ignored.contains(p.as_bytes()), *expected, "table entry {p}");
    }
}
