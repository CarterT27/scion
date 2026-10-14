//! Reusable client scenarios (feature `testing`; P18). Write scenarios (P28) are re-exported
//! from `testing_write`.
//!
//! Every scenario takes a [`Scenario`] (a [`ScionClient`] for the stack under test plus a
//! temporary directory), so the same checks run against `FakeUpstream` (P18), the real server
//! and the daemon (P30, P33). Expectation failures are returned as errors whose message starts
//! with `scenario expectation failed`.

#[allow(unused_imports)] // P00: testing_write is empty until P28.
pub use crate::testing_write::*;

use crate::points;
use crate::wc::{WcOptions, WorkingCopy};
use scion_core::{ErrorCode, FaultAction, FaultRegistry, Mode, ObjectId, Result, ScionError};
use scion_object::RepoPath;
use scion_proto::ScionClient;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A client plus a temporary root for working copies.
pub struct Scenario {
    /// Client for the stack under test.
    pub client: ScionClient,
    /// Temporary directory for working copies.
    pub root: tempfile::TempDir,
}

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

impl Scenario {
    /// A scenario over `client` with a fresh temporary directory.
    pub fn new(client: ScionClient) -> Result<Self> {
        let root = tempfile::tempdir()
            .map_err(|e| crate::fs_safe::io_error("create scenario directory", e))?;
        Ok(Scenario { client, root })
    }

    /// A fresh, not yet existing working-copy directory inside the scenario root.
    pub fn fresh_dir(&self, name: &str) -> PathBuf {
        let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        self.root.path().join(format!("{name}-{n}"))
    }
}

/// A materialized file as seen on disk: mode and content (symlink target for symlinks).
pub type Snapshot = BTreeMap<RepoPath, (Mode, Vec<u8>)>;

/// A scenario expectation failure.
pub fn expectation(msg: impl Into<String>) -> ScionError {
    ScionError::new(
        ErrorCode::InvalidArgument,
        format!("scenario expectation failed: {}", msg.into()),
    )
}

/// Every file under `root` except `.scion` (modes and contents; symlinks are not followed).
pub fn snapshot(root: &Path) -> Result<Snapshot> {
    let mut out = Snapshot::new();
    walk(root, Vec::new(), &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, rel: Vec<u8>, out: &mut Snapshot) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let io = |e| crate::fs_safe::io_error("scan working copy", e);
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(io)?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(io)?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let name = name.as_bytes();
        if rel.is_empty() && name.eq_ignore_ascii_case(b".scion") {
            continue;
        }
        let mut child = rel.clone();
        if !child.is_empty() {
            child.push(b'/');
        }
        child.extend_from_slice(name);
        let md = std::fs::symlink_metadata(e.path()).map_err(io)?;
        let path = RepoPath::parse(&child)?;
        if md.file_type().is_symlink() {
            let t = std::fs::read_link(e.path()).map_err(io)?;
            out.insert(path, (Mode::Symlink, t.as_os_str().as_bytes().to_vec()));
        } else if md.is_dir() {
            walk(&e.path(), child, out)?;
        } else if md.is_file() {
            let mode = if md.permissions().mode() & 0o100 != 0 {
                Mode::Executable
            } else {
                Mode::File
            };
            out.insert(path, (mode, std::fs::read(e.path()).map_err(io)?));
        }
    }
    Ok(())
}

/// `(inode, mtime_ns)` of every file under `root` except `.scion`.
pub fn stat_map(root: &Path) -> Result<BTreeMap<RepoPath, (u64, i128)>> {
    use std::os::unix::fs::MetadataExt;
    let mut out = BTreeMap::new();
    for p in snapshot(root)?.keys() {
        let full =
            root.join(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(p.as_bytes()));
        let md =
            std::fs::symlink_metadata(full).map_err(|e| crate::fs_safe::io_error("stat", e))?;
        out.insert(
            p.clone(),
            (
                md.ino(),
                i128::from(md.mtime()) * 1_000_000_000 + i128::from(md.mtime_nsec()),
            ),
        );
    }
    Ok(out)
}

/// Object ids fetched by a sparse checkout of `dir` at `rev` (SPEC 19.10).
pub async fn sparse_object_set(
    s: &Scenario,
    rev: &str,
    dir: &RepoPath,
) -> Result<BTreeSet<ObjectId>> {
    let root = s.fresh_dir("sparse");
    let wc = WorkingCopy::checkout(
        s.client.clone(),
        &root,
        rev,
        vec![dir.clone()],
        WcOptions::default(),
    )
    .await?;
    Ok(wc.fetched_objects())
}

/// Check out `rev` within `sparse` and return what was materialized (SPEC 19.10: compare with
/// the reference model's materialization).
pub async fn checkout_snapshot(s: &Scenario, rev: &str, sparse: &[RepoPath]) -> Result<Snapshot> {
    let root = s.fresh_dir("checkout");
    WorkingCopy::checkout(
        s.client.clone(),
        &root,
        rev,
        sparse.to_vec(),
        WcOptions::default(),
    )
    .await?;
    snapshot(&root)
}

/// SPEC 11.5 / 19.18: after a checkout and one status, a second status with a warm cache makes
/// zero server calls and hashes nothing. Returns `(calls, hashed)` of the second status.
pub async fn clean_status_zero_calls(s: &Scenario, rev: &str) -> Result<(u64, usize)> {
    let root = s.fresh_dir("status");
    let mut wc = WorkingCopy::checkout(
        s.client.clone(),
        &root,
        rev,
        Vec::new(),
        WcOptions::default(),
    )
    .await?;
    let first = wc.status().await?;
    if !first.is_clean() {
        return Err(expectation("a fresh checkout is not clean"));
    }
    let before = s.client.calls();
    let second = wc.status().await?;
    let calls = s.client.calls() - before;
    if !second.is_clean() {
        return Err(expectation("the second status is not clean"));
    }
    if calls != 0 {
        return Err(expectation(format!(
            "clean status made {calls} server call(s)"
        )));
    }
    Ok((calls, second.hashed))
}

/// SPEC 19.10: goto touches only differing paths (inode and mtime unchanged elsewhere) and the
/// result equals a fresh checkout of `to`. Returns the paths whose content changed.
pub async fn goto_touches_only_diff(s: &Scenario, from: &str, to: &str) -> Result<Vec<RepoPath>> {
    let root = s.fresh_dir("goto");
    let mut wc = WorkingCopy::checkout(
        s.client.clone(),
        &root,
        from,
        Vec::new(),
        WcOptions::default(),
    )
    .await?;
    let before_snap = snapshot(&root)?;
    let before = stat_map(&root)?;
    wc.goto(to, false).await?;
    let after_snap = snapshot(&root)?;
    let after = stat_map(&root)?;
    let expected = checkout_snapshot(s, to, &[]).await?;
    if after_snap != expected {
        return Err(expectation("goto result differs from a fresh checkout"));
    }
    let mut changed = Vec::new();
    for (p, st) in &before {
        let same_content = before_snap.get(p) == after_snap.get(p);
        match after.get(p) {
            Some(st2) if same_content => {
                if st2 != st {
                    return Err(expectation("goto touched a path that did not change"));
                }
            }
            _ => changed.push(p.clone()),
        }
    }
    for p in after.keys() {
        if !before.contains_key(p) {
            changed.push(p.clone());
        }
    }
    changed.sort();
    Ok(changed)
}

/// SPEC 19.10: goto preserves an unrelated dirty file, aborts when a dirty file would be
/// overwritten, and `force` overrides. `touched` must differ between `from` and `to`;
/// `unrelated` must be identical in both.
pub async fn goto_dirty_protection(
    s: &Scenario,
    from: &str,
    to: &str,
    touched: &RepoPath,
    unrelated: &RepoPath,
) -> Result<()> {
    let root = s.fresh_dir("dirty");
    let mut wc = WorkingCopy::checkout(
        s.client.clone(),
        &root,
        from,
        Vec::new(),
        WcOptions::default(),
    )
    .await?;
    let base = wc.state().base;
    wc.safe
        .write_file_atomic(unrelated, b"local unrelated edit\n", Mode::File)?;
    wc.safe
        .write_file_atomic(touched, b"local conflicting edit\n", Mode::File)?;
    match wc.goto(to, false).await {
        Err(e) if e.subcode() == Some(WorkingCopy::SUBCODE_DIRTY) => {}
        Err(e) => return Err(e),
        Ok(_) => return Err(expectation("goto overwrote a dirty file without force")),
    }
    if wc.state().base != base || wc.state().pending.is_some() {
        return Err(expectation("a refused goto changed the state"));
    }
    if wc.safe.read(touched)? != b"local conflicting edit\n" {
        return Err(expectation("a refused goto modified the dirty file"));
    }
    let report = wc.goto(to, true).await?;
    if wc.safe.read(unrelated)? != b"local unrelated edit\n" {
        return Err(expectation("goto did not preserve an unrelated dirty file"));
    }
    let lossy = String::from_utf8_lossy(unrelated.as_bytes()).into_owned();
    if !report.preserved_dirty.contains(&lossy) {
        return Err(expectation("preserved dirty file not reported"));
    }
    let st = wc.status().await?;
    if st.modified != vec![unrelated.clone()] {
        return Err(expectation("status after a forced goto is not as expected"));
    }
    Ok(())
}

/// SPEC 19.10: sparse add materializes exactly the files under `extra`; sparse remove takes
/// them away again and refuses while one of them is dirty (unless forced).
pub async fn sparse_add_remove(
    s: &Scenario,
    rev: &str,
    initial: &RepoPath,
    extra: &RepoPath,
) -> Result<()> {
    let root = s.fresh_dir("sparse-change");
    let mut wc = WorkingCopy::checkout(
        s.client.clone(),
        &root,
        rev,
        vec![initial.clone()],
        WcOptions::default(),
    )
    .await?;
    let before = snapshot(&root)?;
    wc.sparse_add(std::slice::from_ref(extra)).await?;
    let after = snapshot(&root)?;
    let expected = checkout_snapshot(s, rev, &[initial.clone(), extra.clone()]).await?;
    if after != expected {
        return Err(expectation(
            "sparse add result differs from a fresh sparse checkout",
        ));
    }
    let added: Vec<&RepoPath> = after.keys().filter(|p| !before.contains_key(*p)).collect();
    if added
        .iter()
        .any(|p| !(*p == extra || extra.is_ancestor_of(p)))
    {
        return Err(expectation(
            "sparse add materialized a file outside the new prefix",
        ));
    }
    let Some(victim) = added.first().map(|p| (*p).clone()) else {
        return Ok(());
    };
    wc.safe.write_file_atomic(&victim, b"dirty\n", Mode::File)?;
    match wc.sparse_remove(std::slice::from_ref(extra), false).await {
        Err(e) if e.subcode() == Some(WorkingCopy::SUBCODE_DIRTY) => {}
        Err(e) => return Err(e),
        Ok(()) => return Err(expectation("sparse remove removed a dirty file")),
    }
    wc.sparse_remove(std::slice::from_ref(extra), true).await?;
    if snapshot(&root)? != before {
        return Err(expectation(
            "sparse remove did not restore the previous materialization",
        ));
    }
    Ok(())
}

/// SPEC 19.10 / 16.1 class 8: a crash at `point` during `goto(to)` leaves a pending operation
/// that the next command (`status`) completes; the result equals a fresh checkout of `to`.
pub async fn goto_crash_recovery(s: &Scenario, from: &str, to: &str, point: &str) -> Result<()> {
    let root = s.fresh_dir("crash");
    let faults = FaultRegistry::new();
    let opts = WcOptions {
        faults: faults.clone(),
        ..WcOptions::default()
    };
    let mut wc =
        WorkingCopy::checkout(s.client.clone(), &root, from, Vec::new(), opts.clone()).await?;
    faults.arm(point, FaultAction::Crash, Some(1));
    // The goto fails at the crash point (or succeeds when the point is never reached).
    let _ = wc.goto(to, false).await;
    faults.reset();
    let mut reopened = WorkingCopy::open(&root, s.client.clone(), opts)?;
    reopened.status().await?;
    if reopened.state().pending.is_some() {
        return Err(expectation("the pending operation was not completed"));
    }
    if snapshot(&root)? != checkout_snapshot(s, to, &[]).await? {
        return Err(expectation("recovered goto differs from a fresh checkout"));
    }
    Ok(())
}

/// Every client fault point of goto, sparse changes and checkout (SPEC 19.10).
pub fn materialization_points() -> &'static [&'static str] {
    &[
        points::GOTO_AFTER_JOURNAL,
        points::GOTO_MID_WRITE,
        points::SPARSE_MID_WRITE,
        points::CHECKOUT_MID_WRITE,
    ]
}
