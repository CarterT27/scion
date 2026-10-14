//! SPEC 11.5 / 18.7 / 19.10 status correctness (P11): randomized edit sequences match a
//! full-rehash oracle after every step; clean status hashes only racy or stat-changed files;
//! placeholders, ignore rules, scope and inaccessible prefixes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code (PLAN 5.1)

mod local_support;

use local_support::rp;
use proptest::prelude::*;
use scion_core::test_support::proptest_cases;
use scion_core::{EntryInfo, Mode, ObjectId};
use scion_object::RepoPath;
use scion_wc::fs_safe::SafeRoot;
use scion_wc::ignore::IgnoreMatcher;
use scion_wc::index::{IndexEntry, StatCache};
use scion_wc::placeholder_bytes;
use scion_wc::status::{MapTreeLookup, StatusReport, status};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

const GITIGNORE: &[u8] = b"*.log\nbuild/\n";

fn info(content: &[u8], mode: Mode) -> EntryInfo {
    EntryInfo {
        mode,
        id: ObjectId::hash_blob(content),
        size: content.len() as u64,
    }
}

/// Set a path's mtime without following symlinks.
fn set_mtime(path: &Path, t: SystemTime) {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap();
    let ts = rustix::fs::Timestamps {
        last_access: rustix::fs::Timespec {
            tv_sec: i64::try_from(d.as_secs()).unwrap(),
            tv_nsec: d.subsec_nanos().into(),
        },
        last_modification: rustix::fs::Timespec {
            tv_sec: i64::try_from(d.as_secs()).unwrap(),
            tv_nsec: d.subsec_nanos().into(),
        },
    };
    rustix::fs::utimensat(
        rustix::fs::CWD,
        path,
        &ts,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
}

fn past() -> SystemTime {
    SystemTime::now() - Duration::from_secs(100)
}

fn write_file(root: &Path, p: &str, content: &[u8], mode: Mode) {
    let path = root.join(p);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    if path.symlink_metadata().is_ok() {
        std::fs::remove_file(&path).unwrap();
    }
    match mode {
        Mode::Symlink => std::os::unix::fs::symlink(
            std::ffi::OsStr::new(std::str::from_utf8(content).unwrap()),
            &path,
        )
        .unwrap(),
        _ => {
            std::fs::write(&path, content).unwrap();
            let bits = if mode == Mode::Executable {
                0o755
            } else {
                0o644
            };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(bits)).unwrap();
        }
    }
}

// ---- oracle --------------------------------------------------------------------------------

/// Full rehash of everything on disk (std::fs, independent of the status walk).
fn disk_files(root: &Path) -> BTreeMap<Vec<u8>, (ObjectId, Mode)> {
    fn walk(root: &Path, rel: &str, out: &mut BTreeMap<Vec<u8>, (ObjectId, Mode)>) {
        let dir = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(rel)
        };
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().into_string().unwrap();
            if rel.is_empty() && name == ".scion" {
                continue;
            }
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let md = std::fs::symlink_metadata(e.path()).unwrap();
            if md.file_type().is_symlink() {
                let t = std::fs::read_link(e.path()).unwrap();
                let t = t.as_os_str().as_encoded_bytes().to_vec();
                out.insert(child.into_bytes(), (ObjectId::hash_blob(&t), Mode::Symlink));
            } else if md.is_dir() {
                walk(root, &child, out);
            } else if md.is_file() {
                let c = std::fs::read(e.path()).unwrap();
                let mode = if md.permissions().mode() & 0o100 != 0 {
                    Mode::Executable
                } else {
                    Mode::File
                };
                out.insert(child.into_bytes(), (ObjectId::hash_blob(&c), mode));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, "", &mut out);
    out
}

fn covers(prefix: &[u8], p: &[u8]) -> bool {
    prefix.is_empty()
        || p == prefix
        || (p.starts_with(prefix) && p.get(prefix.len()) == Some(&b'/'))
}

fn oracle_ignored(p: &[u8]) -> bool {
    p.ends_with(b".log") || covers(b"build", p)
}

fn oracle(
    root: &Path,
    base: &BTreeMap<RepoPath, EntryInfo>,
    scope: &[&[u8]],
    inaccessible: &[&[u8]],
) -> StatusReport {
    let visible =
        |p: &[u8]| scope.iter().any(|s| covers(s, p)) && !inaccessible.iter().any(|i| covers(i, p));
    let disk = disk_files(root);
    let mut r = StatusReport::default();
    for (p, (id, mode)) in &disk {
        if !visible(p) {
            continue;
        }
        let rp = RepoPath::parse(p).unwrap();
        match base.get(&rp) {
            None if oracle_ignored(p) => {}
            None => r.added.push(rp),
            Some(b) if b.id != *id => r.modified.push(rp),
            Some(b) if b.mode != *mode => r.mode_changed.push(rp),
            Some(_) => {}
        }
    }
    for p in base.keys() {
        if visible(p.as_bytes()) && !disk.contains_key(p.as_bytes()) {
            r.deleted.push(p.clone());
        }
    }
    r
}

fn without_hashed(mut r: StatusReport) -> StatusReport {
    r.hashed = 0;
    r
}

// ---- randomized edit sequences ------------------------------------------------------------

const POOL: &[&str] = &[
    "a",
    "b.txt",
    "d/c",
    "d/e/f",
    "d/e/g.log",
    "g/h",
    "build/out",
    "zz/secret",
    "d/x",
];
const CONTENTS: &[&[u8]] = &[b"", b"x", b"y", b"hello", b"hellp", b"other content\n"];
const TARGETS: &[&str] = &["a", "nowhere", "d/c", "../up", "hello"];

#[derive(Clone, Debug)]
struct Op {
    kind: u8,
    a: usize,
    b: usize,
    v: usize,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    (0..9u8, 0..POOL.len(), 0..POOL.len(), 0..6usize).prop_map(|(kind, a, b, v)| Op {
        kind,
        a,
        b,
        v,
    })
}

fn base_strategy() -> impl Strategy<Value = Vec<(bool, usize, u8)>> {
    proptest::collection::vec((any::<bool>(), 0..6usize, 0..3u8), POOL.len())
}

fn apply(
    root: &Path,
    op: &Op,
    base: &BTreeMap<RepoPath, EntryInfo>,
    base_bytes: &BTreeMap<String, Vec<u8>>,
) {
    let pa = root.join(POOL[op.a]);
    let pb = root.join(POOL[op.b]);
    let exists = |p: &Path| p.symlink_metadata().is_ok();
    let is_link = |p: &Path| {
        p.symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
    };
    // Content: sometimes the base content of the path (reverting makes it clean again).
    let content: Vec<u8> = if op.v == 5 {
        base_bytes
            .get(POOL[op.a])
            .cloned()
            .unwrap_or_else(|| CONTENTS[5].to_vec())
    } else {
        CONTENTS[op.v].to_vec()
    };
    let _ = base;
    match op.kind {
        // create / truncate-and-write
        0 => {
            if is_link(&pa) {
                std::fs::remove_file(&pa).unwrap();
            }
            std::fs::create_dir_all(pa.parent().unwrap()).unwrap();
            std::fs::write(&pa, &content).unwrap();
        }
        // modify in place (no truncation): overwrite a prefix or append
        1 => {
            if exists(&pa) && !is_link(&pa) {
                use std::io::{Seek, Write};
                let mut f = std::fs::OpenOptions::new().write(true).open(&pa).unwrap();
                if op.v.is_multiple_of(2) {
                    f.seek(std::io::SeekFrom::End(0)).unwrap();
                }
                f.write_all(&content).unwrap();
            }
        }
        // delete
        2 => {
            if exists(&pa) {
                std::fs::remove_file(&pa).unwrap();
            }
        }
        // chmod +x / -x
        3 => {
            if exists(&pa) && !is_link(&pa) {
                let bits = if op.v.is_multiple_of(2) { 0o755 } else { 0o644 };
                std::fs::set_permissions(&pa, std::fs::Permissions::from_mode(bits)).unwrap();
            }
        }
        // symlink swap
        4 => {
            if is_link(&pa) {
                std::fs::remove_file(&pa).unwrap();
                std::fs::write(&pa, &content).unwrap();
            } else {
                if exists(&pa) {
                    std::fs::remove_file(&pa).unwrap();
                }
                std::fs::create_dir_all(pa.parent().unwrap()).unwrap();
                std::os::unix::fs::symlink(TARGETS[op.v % TARGETS.len()], &pa).unwrap();
            }
        }
        // rename a -> b
        5 => {
            if exists(&pa) && op.a != op.b {
                std::fs::create_dir_all(pb.parent().unwrap()).unwrap();
                std::fs::rename(&pa, &pb).unwrap();
            }
        }
        // atomic-rename save (editor style)
        6 => {
            std::fs::create_dir_all(pa.parent().unwrap()).unwrap();
            let tmp = pa.with_file_name(".editor-swap");
            std::fs::write(&tmp, &content).unwrap();
            std::fs::rename(&tmp, &pa).unwrap();
        }
        // same-size rewrite with the old mtime restored (same-second edit)
        7 => {
            if exists(&pa) && !is_link(&pa) {
                let md = std::fs::metadata(&pa).unwrap();
                let mut c = std::fs::read(&pa).unwrap();
                if c.is_empty() {
                    c.push(b'z');
                } else {
                    c[0] ^= 0x20;
                }
                std::fs::write(&pa, &c).unwrap();
                set_mtime(&pa, md.modified().unwrap());
            }
        }
        // touch: same content, new mtime (stat change without content change)
        _ => {
            if exists(&pa) && !is_link(&pa) {
                set_mtime(
                    &pa,
                    SystemTime::now() - Duration::from_secs(op.v as u64 + 1),
                );
            }
        }
    }
}

fn run_case(spec: &[(bool, usize, u8)], ops: &[Op]) -> Result<(), TestCaseError> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let scion = root.join(".scion");
    std::fs::create_dir_all(&scion).unwrap();
    std::fs::write(root.join(".gitignore"), GITIGNORE).unwrap();
    std::fs::write(scion.join("junk"), b"never reported").unwrap();

    let mut base = BTreeMap::new();
    let mut base_bytes = BTreeMap::new();
    base.insert(rp(".gitignore"), info(GITIGNORE, Mode::File));
    for (i, (include, c, m)) in spec.iter().enumerate() {
        if !include {
            continue;
        }
        let mode = [Mode::File, Mode::Executable, Mode::Symlink][*m as usize];
        let content: Vec<u8> = if mode == Mode::Symlink {
            TARGETS[*c % TARGETS.len()].as_bytes().to_vec()
        } else {
            CONTENTS[*c].to_vec()
        };
        write_file(root, POOL[i], &content, mode);
        base.insert(rp(POOL[i]), info(&content, mode));
        base_bytes.insert(POOL[i].to_string(), content);
    }
    let lookup = MapTreeLookup(base.clone());
    let safe = SafeRoot::open(root).unwrap();
    let ign = IgnoreMatcher::load(root).unwrap();
    let scope_all = [RepoPath::root()];
    let scope_d = [rp("d")];
    let inacc = [rp("zz")];
    let mut idx = StatCache::default();
    let mut idx_d = StatCache::default();

    let check =
        |idx: &mut StatCache, idx_d: &mut StatCache, step: usize| -> Result<(), TestCaseError> {
            let got = status(&safe, idx, &ign, &scope_all, &inacc, &lookup).unwrap();
            let want = oracle(root, &base, &[b""], &[b"zz"]);
            prop_assert_eq!(
                without_hashed(got.clone()),
                want,
                "step {} (full scope)",
                step
            );
            let got_d = status(&safe, idx_d, &ign, &scope_d, &inacc, &lookup).unwrap();
            let want_d = oracle(root, &base, &[b"d"], &[b"zz"]);
            prop_assert_eq!(without_hashed(got_d), want_d, "step {} (scope d)", step);
            // Index entries never describe out-of-scope or inaccessible paths.
            prop_assert!(
                idx.entries
                    .keys()
                    .all(|p| !p.as_bytes().starts_with(b"zz/"))
            );
            prop_assert!(
                idx_d
                    .entries
                    .keys()
                    .all(|p| p.as_bytes().starts_with(b"d/"))
            );
            Ok(())
        };

    check(&mut idx, &mut idx_d, 0)?;
    for (step, op) in ops.iter().enumerate() {
        apply(root, op, &base, &base_bytes);
        check(&mut idx, &mut idx_d, step + 1)?;
        // Persist and reload the index on alternate steps (the next edit then happens within
        // the same tick as the index write: the racy-clean case).
        if step.is_multiple_of(2) {
            idx.save(&scion).unwrap();
            idx = StatCache::load(&scion).unwrap();
            check(&mut idx, &mut idx_d, step + 1)?;
        }
    }
    Ok(())
}

fn run(cases: u32) {
    let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    runner
        .run(
            &(
                base_strategy(),
                proptest::collection::vec(op_strategy(), 1..12),
            ),
            |(spec, ops)| run_case(&spec, &ops),
        )
        .unwrap();
}

#[test]
fn randomized_edits_match_full_rehash() {
    require_p01!();
    run(proptest_cases(500));
}

#[test]
#[ignore = "spec-sized (SCION_FULL=1): 10^4 randomized edit sequences"]
fn full_randomized_edits_match_full_rehash() {
    require_p01!();
    run(if scion_core::test_support::full_mode() {
        10_000
    } else {
        proptest_cases(10_000)
    });
}

// ---- deterministic properties ---------------------------------------------------------------

struct Fixture {
    dir: tempfile::TempDir,
    safe: SafeRoot,
    ign: IgnoreMatcher,
    base: MapTreeLookup,
}

impl Fixture {
    fn new(files: &[(&str, &[u8], Mode)]) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".scion")).unwrap();
        let mut base = BTreeMap::new();
        for (p, c, m) in files {
            write_file(dir.path(), p, c, *m);
            set_mtime(&dir.path().join(p), past());
            base.insert(rp(p), info(c, *m));
        }
        let safe = SafeRoot::open(dir.path()).unwrap();
        let ign = IgnoreMatcher::load(dir.path()).unwrap();
        Fixture {
            dir,
            safe,
            ign,
            base: MapTreeLookup(base),
        }
    }
    fn root(&self) -> &Path {
        self.dir.path()
    }
    fn scion(&self) -> std::path::PathBuf {
        self.dir.path().join(".scion")
    }
    fn status(&self, idx: &mut StatCache) -> StatusReport {
        status(
            &self.safe,
            idx,
            &self.ign,
            &[RepoPath::root()],
            &[],
            &self.base,
        )
        .unwrap()
    }
}

#[test]
fn clean_status_hashes_only_racy_or_stat_changed_files() {
    require_p01!();
    let f = Fixture::new(&[
        ("a", b"alpha", Mode::File),
        ("d/b", b"beta", Mode::Executable),
        ("d/e/c", b"gamma", Mode::File),
        ("link", b"d/b", Mode::Symlink),
    ]);
    let mut idx = StatCache::default();
    let r = f.status(&mut idx);
    assert!(r.is_clean(), "{r:?}");
    assert_eq!(r.hashed, 4, "empty index: everything hashed");
    assert_eq!(idx.entries.len(), 4);
    idx.save(&f.scion()).unwrap();

    let mut idx = StatCache::load(&f.scion()).unwrap();
    let r = f.status(&mut idx);
    assert!(r.is_clean());
    assert_eq!(r.hashed, 0, "warm index, nothing racy: nothing hashed");

    // A modified file is the only one hashed.
    std::fs::write(f.root().join("d/e/c"), b"GAMMA!").unwrap();
    let r = f.status(&mut idx);
    assert_eq!(r.modified, vec![rp("d/e/c")]);
    assert_eq!(r.hashed, 1);

    // Restore the content with an old mtime: hashed once, clean, refreshed.
    std::fs::write(f.root().join("d/e/c"), b"gamma").unwrap();
    set_mtime(&f.root().join("d/e/c"), past());
    let r = f.status(&mut idx);
    assert!(r.is_clean());
    assert_eq!(r.hashed, 1);
    let r = f.status(&mut idx);
    assert_eq!(r.hashed, 0);

    // A touched file (stat changed, same content) is hashed and found clean.
    set_mtime(&f.root().join("a"), past() + Duration::from_secs(7));
    let r = f.status(&mut idx);
    assert!(r.is_clean());
    assert_eq!(r.hashed, 1);
    assert_eq!(f.status(&mut idx).hashed, 0);

    // A file whose mtime is not older than the index write is racily clean: re-hashed every
    // time, even though nothing changed, until a later index write.
    idx.save(&f.scion()).unwrap();
    let future = SystemTime::UNIX_EPOCH
        + Duration::from_nanos(u64::try_from(idx.written_at_ns).unwrap())
        + Duration::from_secs(3600);
    set_mtime(&f.root().join("d/b"), future);
    let r = f.status(&mut idx);
    assert!(r.is_clean());
    assert_eq!(r.hashed, 1);
    idx.save(&f.scion()).unwrap();
    for _ in 0..2 {
        let r = f.status(&mut idx);
        assert!(r.is_clean());
        assert_eq!(r.hashed, 1, "racy entry re-hashed");
    }
}

#[test]
fn racily_clean_entry_is_rehashed() {
    require_p01!();
    let f = Fixture::new(&[("f", b"aaaa", Mode::File)]);
    // The file changed within the same tick as the recorded stat: same size, and we forge an
    // index entry carrying the *current* stat tuple but the *old* content id.
    std::fs::write(f.root().join("f"), b"bbbb").unwrap();
    let stat = f.safe.stat(&rp("f")).unwrap().unwrap();
    let stale = IndexEntry {
        stat: stat.clone(),
        id: ObjectId::hash_blob(b"aaaa"),
        mode: Mode::File,
        redacted: false,
    };
    // Index written in the same tick as the mtime: racy, so the file is hashed.
    let mut idx = StatCache::default();
    idx.entries.insert(rp("f"), stale.clone());
    idx.written_at_ns = stat.mtime_ns;
    let r = f.status(&mut idx);
    assert_eq!(r.modified, vec![rp("f")]);
    assert_eq!(r.hashed, 1);
    // Index written strictly later: the stat tuple is trusted (no hashing).
    let mut idx = StatCache::default();
    idx.entries.insert(rp("f"), stale);
    idx.written_at_ns = stat.mtime_ns + 1;
    let r = f.status(&mut idx);
    assert!(r.is_clean());
    assert_eq!(r.hashed, 0);
}

#[test]
fn symlink_materialized_placeholder_is_clean() {
    require_p01!();
    let f = Fixture::new(&[("plain", b"p", Mode::File)]);
    // A redacted symlink entry whose placeholder was written as a symlink target
    // (`write_file_atomic(.., Mode::Symlink)` delegates to `symlink`).
    let target = b"secret/target";
    let id = ObjectId::hash_blob(target);
    let mut base = f.base.0.clone();
    base.insert(
        rp("rl"),
        EntryInfo {
            mode: Mode::Symlink,
            id,
            size: target.len() as u64,
        },
    );
    let lookup = MapTreeLookup(base);
    f.safe.symlink(&rp("rl"), &placeholder_bytes(&id)).unwrap();
    let run = |idx: &mut StatCache| {
        status(&f.safe, idx, &f.ign, &[RepoPath::root()], &[], &lookup).unwrap()
    };
    let mut idx = StatCache::default();
    let r = run(&mut idx);
    assert!(r.is_clean(), "{r:?}");
    assert!(idx.entries.get(&rp("rl")).unwrap().redacted);
    idx.save(&f.scion()).unwrap();
    let mut idx = StatCache::load(&f.scion()).unwrap();
    let r = run(&mut idx);
    assert!(r.is_clean(), "{r:?}");
    assert_eq!(r.hashed, 0);
    // Retargeting the symlink is an edit of the placeholder.
    f.safe.symlink(&rp("rl"), b"elsewhere").unwrap();
    let r = run(&mut idx);
    assert_eq!(r.modified, vec![rp("rl")]);
    assert_eq!(r.redacted_dirty, vec![rp("rl")]);
}

#[test]
fn unreadable_directories() {
    require_p01!();
    let f = Fixture::new(&[("locked/a", b"a", Mode::File), ("top", b"t", Mode::File)]);
    std::fs::create_dir_all(f.root().join("junk")).unwrap();
    std::fs::write(f.root().join("junk/new"), b"n").unwrap();
    let chmod = |p: &str, bits: u32| {
        std::fs::set_permissions(f.root().join(p), std::fs::Permissions::from_mode(bits)).unwrap()
    };
    chmod("junk", 0o000);
    if std::fs::read_dir(f.root().join("junk")).is_ok() {
        chmod("junk", 0o755);
        eprintln!("SKIP: running with privileges that bypass directory permissions");
        return;
    }
    // An unreadable untracked directory is skipped (nothing reported under it).
    let r = f.status(&mut StatCache::default());
    assert!(r.is_clean(), "{r:?}");
    // An unreadable directory holding tracked files is an error, never "deleted".
    chmod("locked", 0o000);
    let err = status(
        &f.safe,
        &mut StatCache::default(),
        &f.ign,
        &[RepoPath::root()],
        &[],
        &f.base,
    )
    .unwrap_err();
    assert_eq!(err.code, scion_core::ErrorCode::PermissionDenied);
    // Out of scope, it is not visited at all.
    let r = status(
        &f.safe,
        &mut StatCache::default(),
        &f.ign,
        &[rp("top")],
        &[],
        &f.base,
    )
    .unwrap();
    assert!(r.is_clean(), "{r:?}");
    chmod("locked", 0o755);
    chmod("junk", 0o755);
    assert!(f.status(&mut StatCache::default()).added == vec![rp("junk/new")]);
}

#[test]
fn placeholders_ignore_scope_and_inaccessible() {
    require_p01!();
    let secret = b"top secret contents";
    let f = Fixture::new(&[
        ("keep", b"k", Mode::File),
        (".gitignore", b"*.o\nout/\n", Mode::File),
        ("tracked.o", b"obj", Mode::File),
        ("priv/x", b"p", Mode::File),
        ("sub/y", b"y", Mode::File),
    ]);
    // A redacted file: the base has the real blob, the disk has its placeholder.
    let red_id = ObjectId::hash_blob(secret);
    let mut base = f.base.0.clone();
    base.insert(
        rp("red"),
        EntryInfo {
            mode: Mode::File,
            id: red_id,
            size: secret.len() as u64,
        },
    );
    std::fs::write(f.root().join("red"), placeholder_bytes(&red_id)).unwrap();
    let lookup = MapTreeLookup(base);
    let ign = IgnoreMatcher::load(f.root()).unwrap();
    std::fs::write(f.root().join("new.o"), b"ignored").unwrap();
    std::fs::create_dir_all(f.root().join("out/deep")).unwrap();
    std::fs::write(f.root().join("out/deep/z"), b"ignored").unwrap();
    std::fs::write(f.root().join("tracked.o"), b"changed").unwrap();
    std::fs::write(f.root().join("priv/new"), b"invisible").unwrap();
    std::fs::write(f.root().join(".scion/stuff"), b"invisible").unwrap();
    std::fs::write(f.root().join("added"), b"new").unwrap();
    // A symlinked directory is a symlink entry, never descended into.
    std::os::unix::fs::symlink("sub", f.root().join("sublink")).unwrap();

    let mut idx = StatCache::default();
    let run = |idx: &mut StatCache, scope: &[RepoPath]| {
        status(&f.safe, idx, &ign, scope, &[rp("priv")], &lookup).unwrap()
    };
    let r = run(&mut idx, &[RepoPath::root()]);
    assert_eq!(r.added, vec![rp("added"), rp("sublink")]);
    assert_eq!(
        r.modified,
        vec![rp("tracked.o")],
        "tracked files are never ignored"
    );
    assert!(r.deleted.is_empty() && r.mode_changed.is_empty() && r.redacted_dirty.is_empty());
    let e = idx.entries.get(&rp("red")).unwrap();
    assert!(e.redacted, "unmodified placeholder is clean and flagged");
    assert_eq!(e.id, red_id);
    assert!(
        !idx.entries
            .keys()
            .any(|p| p.as_bytes().starts_with(b"priv"))
    );

    // Scope restriction.
    let r = run(&mut StatCache::default(), &[rp("sub")]);
    assert!(r.is_clean(), "{r:?}");
    assert_eq!(r.hashed, 1);
    let r = run(&mut StatCache::default(), &[]);
    assert!(r.is_clean() && r.hashed == 0, "empty scope covers nothing");

    // Editing the placeholder: modified and redacted_dirty.
    idx.save(&f.scion()).unwrap();
    let mut idx = StatCache::load(&f.scion()).unwrap();
    std::fs::write(f.root().join("red"), b"my own new content").unwrap();
    let r = run(&mut idx, &[RepoPath::root()]);
    assert_eq!(r.modified, vec![rp("red"), rp("tracked.o")]);
    assert_eq!(r.redacted_dirty, vec![rp("red")]);
    // Restoring the placeholder makes it clean again.
    std::fs::write(f.root().join("red"), placeholder_bytes(&red_id)).unwrap();
    let r = run(&mut idx, &[RepoPath::root()]);
    assert!(r.redacted_dirty.is_empty());
    assert_eq!(r.modified, vec![rp("tracked.o")]);

    // Deletion and mode change.
    std::fs::remove_file(f.root().join("keep")).unwrap();
    std::fs::set_permissions(
        f.root().join("sub/y"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let r = run(&mut idx, &[RepoPath::root()]);
    assert_eq!(r.deleted, vec![rp("keep")]);
    assert_eq!(r.mode_changed, vec![rp("sub/y")]);
    assert!(
        !idx.entries.contains_key(&rp("keep")),
        "vanished entries are dropped"
    );
}
