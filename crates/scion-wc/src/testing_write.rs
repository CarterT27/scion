//! Reusable write scenarios (commit, amend, rebase with conflicts, land, crash recovery) for
//! P30/P33 (feature `testing`; P28). Re-exported from [`crate::testing`].
//!
//! Every scenario works through a [`Scenario`] client only, against a ref `r` that exists and
//! that the client's principal may land on (read and write on `/`). Expectation failures are
//! errors whose message starts with `scenario expectation failed`. New file contents carry a
//! random nonce, so the blobs a scenario creates are new to the repository even when other
//! scenarios ran against it before.

pub use crate::commit::CommitReport;

use crate::commit::rpc;
use crate::points;
use crate::state::PendingOperation;
use crate::testing::{Scenario, Snapshot, checkout_snapshot, expectation, snapshot};
use crate::wc::{WcOptions, WorkingCopy};
use scion_core::{
    ChangeId, EntryInfo, ErrorCode, FaultAction, FaultRegistry, Mode, ObjectId, Result,
};
use scion_object::{FlatTree, RefName, RepoPath, file_diff};
use scion_proto::convert::required_id;
use scion_proto::{ScionClient, error_from_status, v1};
use std::collections::BTreeSet;
use std::sync::Arc;

/// A file to write: `(path, mode, content or symlink target)`.
pub type FileSpec<'a> = (&'a str, Mode, &'a [u8]);

fn rp(p: &str) -> Result<RepoPath> {
    RepoPath::parse(p.as_bytes())
}

/// A random hex nonce for unique file contents.
pub fn nonce() -> String {
    ChangeId::random().to_hex()
}

/// Write files into a working copy through its safe root (SPEC 11.3 path rules).
pub fn write_files(wc: &WorkingCopy, files: &[FileSpec<'_>]) -> Result<()> {
    for (p, m, c) in files {
        wc.safe.write_file_atomic(&rp(p)?, c, *m)?;
    }
    Ok(())
}

/// Remove files from a working copy.
pub fn remove_files(wc: &WorkingCopy, paths: &[&str]) -> Result<()> {
    for p in paths {
        wc.safe.remove(&rp(p)?)?;
    }
    Ok(())
}

/// Check out `rev` (whole tree) into a fresh directory of the scenario, with `faults`.
pub async fn checkout_at(
    s: &Scenario,
    rev: &str,
    faults: Option<Arc<FaultRegistry>>,
) -> Result<WorkingCopy> {
    let opts = WcOptions {
        faults: faults.unwrap_or_default(),
        ..WcOptions::default()
    };
    WorkingCopy::checkout(
        s.client.clone(),
        &s.fresh_dir("write"),
        rev,
        Vec::new(),
        opts,
    )
    .await
}

/// The current target of ref `r` (SPEC 7.3 ResolveRef).
pub async fn ref_target(client: &ScionClient, r: &RefName) -> Result<ObjectId> {
    let resp = client
        .objects()
        .resolve_ref(v1::ResolveRefRequest {
            name: r.as_str().to_owned(),
            at_version: None,
        })
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    required_id(&resp.target, "target")
}

/// Commit `files` (and remove `removed`) on top of ref `r` through a working copy, then land
/// it; returns the new ref target (SPEC 11.6, 11.7).
pub async fn seed_files(
    s: &Scenario,
    r: &RefName,
    files: &[FileSpec<'_>],
    removed: &[&str],
) -> Result<ObjectId> {
    let mut wc = checkout_at(s, r.as_str(), None).await?;
    write_files(&wc, files)?;
    remove_files(&wc, removed)?;
    wc.commit(b"seed\n", b"Seeder <seed@example.com>", None)
        .await?;
    let report = wc.land(r).await?;
    if !report.moved {
        return Err(expectation("seed land did not move the working copy"));
    }
    ref_target(&s.client, r).await
}

/// The file-level tree of a snapshot (SPEC 4.3.4 FlatTree).
pub fn flat_of(snap: &Snapshot) -> FlatTree {
    snap.iter()
        .map(|(p, (m, c))| {
            (
                p.clone(),
                EntryInfo {
                    mode: *m,
                    id: ObjectId::hash_blob(c),
                    size: c.len() as u64,
                },
            )
        })
        .collect()
}

/// The snapshot of a working copy's files (outside `.scion`).
pub fn wc_snapshot(wc: &WorkingCopy) -> Result<Snapshot> {
    snapshot(wc.root())
}

/// Check that the base's tree is exactly what is on disk and status is clean.
async fn expect_committed_state(s: &Scenario, wc: &mut WorkingCopy, what: &str) -> Result<()> {
    let st = wc.status().await?;
    if !st.is_clean() {
        return Err(expectation(format!("{what}: status is not clean: {st:?}")));
    }
    if wc.state().pending.is_some() {
        return Err(expectation(format!("{what}: a pending operation remains")));
    }
    let disk = wc_snapshot(wc)?;
    let fresh = checkout_snapshot(s, &wc.state().base.to_text(), &[]).await?;
    if disk != fresh {
        return Err(expectation(format!(
            "{what}: the base tree differs from the working files"
        )));
    }
    Ok(())
}

/// SPEC 19.10: commit uploads only missing blobs (the UploadObjects item count equals the
/// number of distinct new blobs) and sends a delta equal to FileDiff(base tree, working tree);
/// the created changeset carries exactly that delta. Returns the commit report.
pub async fn commit_uploads_only_missing(s: &Scenario, r: &RefName) -> Result<CommitReport> {
    let n = nonce();
    let alpha = format!("alpha {n}\n");
    let beta = format!("beta {n}\n");
    seed_files(
        s,
        r,
        &[
            ("w/a.txt", Mode::File, alpha.as_bytes()),
            ("w/dir/b.txt", Mode::File, beta.as_bytes()),
        ],
        &[],
    )
    .await?;
    let mut wc = checkout_at(s, r.as_str(), None).await?;
    let before = flat_of(&wc_snapshot(&wc)?);
    let shared = format!("new one {n}\n");
    let tool = format!("#!/bin/sh\necho {n}\n");
    let beta2 = format!("beta 2 {n}\n");
    let link = format!("a.txt.{n}");
    write_files(
        &wc,
        &[
            ("w/copy.txt", Mode::File, alpha.as_bytes()),
            ("w/n1.txt", Mode::File, shared.as_bytes()),
            ("w/dir/n2.txt", Mode::File, shared.as_bytes()),
            ("w/tool.sh", Mode::Executable, tool.as_bytes()),
            ("w/link", Mode::Symlink, link.as_bytes()),
            ("w/dir/b.txt", Mode::File, beta2.as_bytes()),
        ],
    )?;
    remove_files(&wc, &["w/a.txt"])?;
    let after = flat_of(&wc_snapshot(&wc)?);
    let expected = file_diff(&before, &after);
    let known: BTreeSet<ObjectId> = before.values().map(|e| e.id).collect();
    let new_blobs: BTreeSet<ObjectId> = after
        .values()
        .map(|e| e.id)
        .filter(|id| !known.contains(id))
        .collect();
    let base = wc.state().base;
    let report = wc
        .commit_with_report(b"commit scenario\n", b"Alice <alice@example.com>", None)
        .await?;
    if report.delta != expected {
        return Err(expectation(format!(
            "commit delta differs from FileDiff: {:?} vs {:?}",
            report.delta, expected
        )));
    }
    if report.uploaded != new_blobs.len() {
        return Err(expectation(format!(
            "commit uploaded {} blob(s), {} are new",
            report.uploaded,
            new_blobs.len()
        )));
    }
    if report.parents != vec![base] || wc.state().base != report.changeset {
        return Err(expectation("commit parents or new base are wrong"));
    }
    let server = rpc::fetch_changeset(&s.client, &report.changeset).await?;
    if server.delta != expected || server.parents != vec![base] {
        return Err(expectation(
            "the created changeset does not carry the delta",
        ));
    }
    expect_committed_state(s, &mut wc, "commit").await?;
    Ok(report)
}

/// SPEC 19.10 / 11.6: amend preserves the change id and the parents, recomputes the delta
/// against the first parent (a change reverted before amending disappears), keeps the Meta
/// fields, and replaces only the message when a new one is given.
pub async fn amend_preserves_change_id(s: &Scenario, r: &RefName) -> Result<()> {
    let n = nonce();
    let v1 = format!("v1 {n}\n");
    seed_files(s, r, &[("am/f.txt", Mode::File, v1.as_bytes())], &[]).await?;
    let mut wc = checkout_at(s, r.as_str(), None).await?;
    let v2 = format!("v2 {n}\n");
    let added = format!("added {n}\n");
    write_files(
        &wc,
        &[
            ("am/f.txt", Mode::File, v2.as_bytes()),
            ("am/new.txt", Mode::File, added.as_bytes()),
        ],
    )?;
    let author = b"Amy <amy@example.com>";
    let first = wc
        .commit_with_report(b"first message\n", author, None)
        .await?;
    let first_meta = wc
        .fetch_meta(
            &rpc::fetch_changeset(&s.client, &first.changeset)
                .await?
                .meta,
        )
        .await?;
    // Revert f.txt and add another file, then amend.
    let more = format!("more {n}\n");
    write_files(
        &wc,
        &[
            ("am/f.txt", Mode::File, v1.as_bytes()),
            ("am/more.txt", Mode::File, more.as_bytes()),
        ],
    )?;
    let second = wc.amend_with_report(None).await?;
    if second.change_id != first.change_id || second.parents != first.parents {
        return Err(expectation("amend changed the change id or the parents"));
    }
    if second.changeset == first.changeset {
        return Err(expectation("amend did not create a new changeset"));
    }
    let paths: Vec<&[u8]> = second.delta.iter().map(|d| d.path.as_bytes()).collect();
    if paths != vec![&b"am/more.txt"[..], b"am/new.txt"] {
        return Err(expectation(format!(
            "amend delta is not the composed change: {paths:?}"
        )));
    }
    let cs = rpc::fetch_changeset(&s.client, &second.changeset).await?;
    let meta = wc.fetch_meta(&cs.meta).await?;
    if meta.author != first_meta.author
        || meta.author_time != first_meta.author_time
        || meta.message != first_meta.message
        || cs.change_id != first.change_id
    {
        return Err(expectation("amend did not reuse the original Meta fields"));
    }
    expect_committed_state(s, &mut wc, "amend").await?;
    let third = wc.amend_with_report(Some(b"new message\n")).await?;
    let meta3 = wc
        .fetch_meta(
            &rpc::fetch_changeset(&s.client, &third.changeset)
                .await?
                .meta,
        )
        .await?;
    if third.change_id != first.change_id
        || third.root_tree != second.root_tree
        || meta3.message != b"new message\n"
        || meta3.author != first_meta.author
        || meta3.author_time != first_meta.author_time
    {
        return Err(expectation("amend with a message is not as expected"));
    }
    expect_committed_state(s, &mut wc, "amend with message").await
}

/// SPEC 19.10 / 11.7 full conflict flow: markers for text conflicts; the ours version plus
/// side files for binary, symlink and modify/delete conflicts; clean entries (including a
/// clean content merge) written; `resolve` refuses remaining markers unless forced; commit
/// refuses unresolved conflicts; the continuing commit preserves change id, author, time and
/// message; `--abort` restores the original exactly.
pub async fn conflict_flow(s: &Scenario, r: &RefName) -> Result<()> {
    let n = nonce();
    let txt = |v: &str| format!("{n}\n{v}\nend\n");
    let (t_base, t_up, t_local) = (txt("base"), txt("upstream"), txt("local"));
    let merge_base = format!("1 {n}\n2\n3\n4\n5\n");
    let merge_up = format!("1 up {n}\n2\n3\n4\n5\n");
    let merge_local = format!("1 {n}\n2\n3\n4\n5 local\n");
    let merged = format!("1 up {n}\n2\n3\n4\n5 local\n");
    let bin = |v: &str| format!("\0bin {n} {v}").into_bytes();
    let (b_base, b_up, b_local) = (bin("base"), bin("up"), bin("local"));
    let link = |v: &str| format!("target-{v}-{n}");
    let (l_base, l_up, l_local) = (link("base"), link("up"), link("local"));
    let md_base = format!("md {n}\n");
    let md_up = format!("md up {n}\n");
    let gone_base = format!("gone {n}\n");
    let gone_local = format!("gone local {n}\n");
    let clean_local = format!("clean local {n}\n");

    let start = seed_files(
        s,
        r,
        &[
            ("cf/text.txt", Mode::File, t_base.as_bytes()),
            ("cf/merge.txt", Mode::File, merge_base.as_bytes()),
            ("cf/bin.dat", Mode::File, &b_base),
            ("cf/link", Mode::Symlink, l_base.as_bytes()),
            ("cf/md.txt", Mode::File, md_base.as_bytes()),
            ("cf/gone.txt", Mode::File, gone_base.as_bytes()),
        ],
        &[],
    )
    .await?;

    // The local side, from the same start.
    let mut local = checkout_at(s, &start.to_text(), None).await?;
    // Upstream lands its version first.
    seed_files(
        s,
        r,
        &[
            ("cf/text.txt", Mode::File, t_up.as_bytes()),
            ("cf/merge.txt", Mode::File, merge_up.as_bytes()),
            ("cf/bin.dat", Mode::File, &b_up),
            ("cf/link", Mode::Symlink, l_up.as_bytes()),
            ("cf/md.txt", Mode::File, md_up.as_bytes()),
        ],
        &["cf/gone.txt"],
    )
    .await?;
    let onto = ref_target(&s.client, r).await?;

    write_files(
        &local,
        &[
            ("cf/text.txt", Mode::File, t_local.as_bytes()),
            ("cf/merge.txt", Mode::File, merge_local.as_bytes()),
            ("cf/bin.dat", Mode::File, &b_local),
            ("cf/link", Mode::Symlink, l_local.as_bytes()),
            ("cf/gone.txt", Mode::File, gone_local.as_bytes()),
            ("cf/clean.txt", Mode::File, clean_local.as_bytes()),
        ],
    )?;
    remove_files(&local, &["cf/md.txt"])?;
    let author = b"Lou <lou@example.com>";
    let message = b"local change\n";
    let original = local.commit_with_report(message, author, None).await?;
    let original_snap = wc_snapshot(&local)?;
    let original_meta = local
        .fetch_meta(
            &rpc::fetch_changeset(&s.client, &original.changeset)
                .await?
                .meta,
        )
        .await?;

    // --- rebase: conflicts are materialized ---
    let report = local.rebase(r.as_str()).await?;
    let want: Vec<String> = [
        "cf/bin.dat",
        "cf/gone.txt",
        "cf/link",
        "cf/md.txt",
        "cf/text.txt",
    ]
    .iter()
    .map(|p| (*p).to_owned())
    .collect();
    if report.conflicts != want || report.head.is_some() {
        return Err(expectation(format!(
            "rebase conflicts are not as expected: {:?}",
            report.conflicts
        )));
    }
    if local.state().base != onto {
        return Err(expectation(
            "the paused rebase is not at onto's merged state",
        ));
    }
    let disk = wc_snapshot(&local)?;
    let file = |p: &str| disk.get(&RepoPath::parse(p.as_bytes()).ok()?).cloned();
    let markers =
        scion_merge::render_markers(t_base.as_bytes(), t_up.as_bytes(), t_local.as_bytes());
    if file("cf/text.txt") != Some((Mode::File, markers)) {
        return Err(expectation("text conflict markers were not written"));
    }
    if file("cf/merge.txt") != Some((Mode::File, merged.clone().into_bytes())) {
        return Err(expectation("the clean content merge was not written"));
    }
    if file("cf/clean.txt") != Some((Mode::File, clean_local.clone().into_bytes())) {
        return Err(expectation("a clean entry was not written"));
    }
    if file("cf/bin.dat") != Some((Mode::File, b_up.clone()))
        || file("cf/link") != Some((Mode::Symlink, l_up.clone().into_bytes()))
        || file("cf/md.txt") != Some((Mode::File, md_up.clone().into_bytes()))
        || file("cf/gone.txt").is_some()
    {
        return Err(expectation(
            "non-text conflicts do not hold the ours version",
        ));
    }
    let side = |p: &str, sd: &str| -> Option<Vec<u8>> {
        std::fs::read(local.conflict_side_path(&RepoPath::parse(p.as_bytes()).ok()?, sd)).ok()
    };
    let sides_ok = side("cf/bin.dat", "base") == Some(b_base.clone())
        && side("cf/bin.dat", "ours") == Some(b_up.clone())
        && side("cf/bin.dat", "theirs") == Some(b_local.clone())
        && side("cf/link", "base") == Some(l_base.clone().into_bytes())
        && side("cf/link", "ours") == Some(l_up.clone().into_bytes())
        && side("cf/link", "theirs") == Some(l_local.clone().into_bytes())
        && side("cf/md.txt", "base") == Some(md_base.clone().into_bytes())
        && side("cf/md.txt", "ours") == Some(md_up.clone().into_bytes())
        && side("cf/md.txt", "theirs").is_none()
        && side("cf/gone.txt", "base") == Some(gone_base.clone().into_bytes())
        && side("cf/gone.txt", "ours").is_none()
        && side("cf/gone.txt", "theirs") == Some(gone_local.clone().into_bytes())
        && side("cf/text.txt", "base").is_none();
    if !sides_ok {
        return Err(expectation("conflict side files are not as expected"));
    }

    // --- resolve ---
    let text = rp("cf/text.txt")?;
    match local.resolve(&text, false) {
        Err(e) if e.subcode() == Some(WorkingCopy::SUBCODE_MARKERS_REMAIN) => {}
        other => {
            return Err(expectation(format!(
                "resolve accepted remaining markers: {other:?}"
            )));
        }
    }
    local.resolve(&text, true)?;
    match local.commit(b"ignored", b"ignored", None).await {
        Err(e) if e.subcode() == Some(WorkingCopy::SUBCODE_UNRESOLVED) => {}
        other => {
            return Err(expectation(format!(
                "commit accepted unresolved conflicts: {other:?}"
            )));
        }
    }
    let resolved_text = format!("{n}\nupstream and local\nend\n");
    write_files(
        &local,
        &[
            ("cf/text.txt", Mode::File, resolved_text.as_bytes()),
            ("cf/bin.dat", Mode::File, &b_local),
        ],
    )?;
    remove_files(&local, &["cf/md.txt"])?;
    for p in [
        "cf/text.txt",
        "cf/bin.dat",
        "cf/link",
        "cf/md.txt",
        "cf/gone.txt",
    ] {
        local.resolve(&rp(p)?, false)?;
    }
    let cont = local
        .commit_with_report(b"ignored", b"ignored", None)
        .await?;
    let cs = rpc::fetch_changeset(&s.client, &cont.changeset).await?;
    let meta = local.fetch_meta(&cs.meta).await?;
    if cs.change_id != original.change_id
        || meta.author != original_meta.author
        || meta.author_time != original_meta.author_time
        || meta.message != original_meta.message
        || cs.parents != vec![onto]
    {
        return Err(expectation(
            "the continuing commit did not preserve change id, author and message",
        ));
    }
    if local.state().pending.is_some() || local.state().base != cont.changeset {
        return Err(expectation("the rebase did not complete after the commit"));
    }
    if local
        .conflict_side_path(&rp("cf/bin.dat")?, "base")
        .exists()
    {
        return Err(expectation("conflict side files were not cleaned up"));
    }
    expect_committed_state(s, &mut local, "conflict continuation").await?;

    // --- abort restores the original exactly ---
    let mut again = checkout_at(s, &original.changeset.to_text(), None).await?;
    let again_report = again.rebase(r.as_str()).await?;
    if again_report.conflicts.is_empty() {
        return Err(expectation("the second rebase did not conflict"));
    }
    write_files(&again, &[("cf/scratch.txt", Mode::File, b"scratch\n")])?;
    again.rebase_abort().await?;
    if again.state().base != original.changeset || again.state().pending.is_some() {
        return Err(expectation(
            "abort did not return to the original changeset",
        ));
    }
    if wc_snapshot(&again)? != original_snap {
        return Err(expectation(
            "abort did not restore the original files exactly",
        ));
    }
    if again.root().join(".scion").join("conflicts").exists() {
        return Err(expectation("abort left conflict side files"));
    }
    if !again.status().await?.is_clean() {
        return Err(expectation("status after abort is not clean"));
    }
    Ok(())
}

/// SPEC 19.10 / 11.7: land moves the base to the landed result and sets the tracking ref,
/// also when the ref advanced meanwhile (the result is then a rebased changeset).
pub async fn land_moves_base(s: &Scenario, r: &RefName) -> Result<()> {
    let n = nonce();
    let mine = format!("mine {n}\n");
    let theirs = format!("theirs {n}\n");
    let mut wc = checkout_at(s, r.as_str(), None).await?;
    write_files(&wc, &[("land/mine.txt", Mode::File, mine.as_bytes())])?;
    let committed = wc.commit(b"mine\n", b"Me <me@example.com>", None).await?;
    // Someone else lands first.
    seed_files(
        s,
        r,
        &[("land/theirs.txt", Mode::File, theirs.as_bytes())],
        &[],
    )
    .await?;
    let report = wc.land(r).await?;
    let tip = ref_target(&s.client, r).await?;
    if !report.moved || report.target != tip.to_text() || report.head != tip.to_text() {
        return Err(expectation("land report is not as expected"));
    }
    if wc.state().base != tip || wc.state().base == committed {
        return Err(expectation(
            "land did not move the base to the landed result",
        ));
    }
    if wc.state().tracking_ref.as_ref() != Some(r) {
        return Err(expectation("land did not set the tracking ref"));
    }
    let disk = wc_snapshot(&wc)?;
    if !disk.contains_key(&rp("land/mine.txt")?) || !disk.contains_key(&rp("land/theirs.txt")?) {
        return Err(expectation("the landed result is not materialized"));
    }
    expect_committed_state(s, &mut wc, "land").await?;
    // Landing again is idempotent and keeps the base.
    let again = wc.land(r).await?;
    if wc.state().base != tip || again.target != tip.to_text() {
        return Err(expectation("a repeated land moved the base"));
    }
    Ok(())
}

/// Every write-layer fault point (SPEC 19.10 crash injection).
pub fn write_points() -> &'static [&'static str] {
    &[
        points::COMMIT_AFTER_JOURNAL,
        points::COMMIT_AFTER_UPLOAD,
        points::COMMIT_AFTER_CREATE,
        points::REBASE_AFTER_JOURNAL,
        points::REBASE_MID_MEMBER,
        points::REBASE_AFTER_CONFLICT_WRITE,
    ]
}

/// How a crashed operation was recovered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovered {
    /// The operation was completed.
    Completed,
    /// The operation was rolled back.
    RolledBack,
    /// A rebase was resumed and is paused on conflicts.
    PausedOnConflicts,
}

/// SPEC 19.10 / 16.1 class 8: a crash at `point` during a commit leaves a pending operation
/// that the next command (`status`) completes or rolls back, without touching the files;
/// afterwards status is consistent (clean, with the base tree equal to the files, when
/// completed; the change still reported when rolled back) and committing again succeeds.
pub async fn commit_crash_recovery(s: &Scenario, r: &RefName, point: &str) -> Result<Recovered> {
    let n = nonce();
    let faults = FaultRegistry::new();
    let mut wc = checkout_at(s, r.as_str(), Some(faults.clone())).await?;
    let content = format!("crash {n}\n");
    write_files(&wc, &[("crash/new.txt", Mode::File, content.as_bytes())])?;
    let base = wc.state().base;
    let disk = wc_snapshot(&wc)?;
    faults.arm(point, FaultAction::Crash, Some(1));
    let crashed = wc.commit(b"crash\n", b"Cy <cy@example.com>", None).await;
    faults.reset();
    if crashed.is_ok() || faults.hits(point) == 0 {
        return Err(expectation(format!("the commit did not crash at {point}")));
    }
    if !matches!(wc.state().pending, Some(PendingOperation::Commit { .. })) {
        return Err(expectation("the crash left no pending commit"));
    }
    let root = wc.root().to_path_buf();
    drop(wc);
    let opts = WcOptions {
        faults,
        ..WcOptions::default()
    };
    let mut wc = WorkingCopy::open(&root, s.client.clone(), opts)?;
    let st = wc.status().await?;
    if wc.state().pending.is_some() {
        return Err(expectation("recovery left the pending commit"));
    }
    if wc_snapshot(&wc)? != disk {
        return Err(expectation("recovery touched the working files"));
    }
    let outcome = if wc.state().base == base {
        if st.added != vec![rp("crash/new.txt")?] || !st.modified.is_empty() {
            return Err(expectation("a rolled-back commit's change is not reported"));
        }
        wc.commit(b"crash again\n", b"Cy <cy@example.com>", None)
            .await?;
        Recovered::RolledBack
    } else {
        let cs = rpc::fetch_changeset(&s.client, &wc.state().base).await?;
        if cs.parents != vec![base] || !st.is_clean() {
            return Err(expectation("a completed commit is not consistent"));
        }
        Recovered::Completed
    };
    expect_committed_state(s, &mut wc, "commit recovery").await?;
    Ok(outcome)
}

/// SPEC 19.10 / 16.1 class 8: a crash at `point` during a rebase of a two-member stack leaves
/// a pending operation that the next command (`status`) resumes: without a conflict it
/// completes (both members rebased, in order, change ids kept, files equal to the result);
/// at `wc.rebase.after_conflict_write` it ends paused on the conflict, which then resolves and
/// commits normally.
pub async fn rebase_crash_recovery(s: &Scenario, r: &RefName, point: &str) -> Result<Recovered> {
    let n = nonce();
    let conflict = point == points::REBASE_AFTER_CONFLICT_WRITE;
    let f0 = format!("f0 {n}\n");
    let h0 = format!("h0 {n}\n");
    let start = seed_files(
        s,
        r,
        &[
            ("rc/f.txt", Mode::File, f0.as_bytes()),
            ("rc/h.txt", Mode::File, h0.as_bytes()),
        ],
        &[],
    )
    .await?;
    let faults = FaultRegistry::new();
    let mut wc = checkout_at(s, &start.to_text(), Some(faults.clone())).await?;
    let g_up = format!("g up {n}\n");
    let h_up = format!("h up {n}\n");
    let mut up: Vec<FileSpec<'_>> = vec![("rc/g.txt", Mode::File, g_up.as_bytes())];
    if conflict {
        up.push(("rc/h.txt", Mode::File, h_up.as_bytes()));
    }
    seed_files(s, r, &up, &[]).await?;
    let onto = ref_target(&s.client, r).await?;
    let f1 = format!("f1 {n}\n");
    let h1 = format!("h1 {n}\n");
    write_files(&wc, &[("rc/f.txt", Mode::File, f1.as_bytes())])?;
    let m1 = wc
        .commit_with_report(b"m1\n", b"Rae <rae@example.com>", None)
        .await?;
    write_files(&wc, &[("rc/h.txt", Mode::File, h1.as_bytes())])?;
    let m2 = wc
        .commit_with_report(b"m2\n", b"Rae <rae@example.com>", None)
        .await?;
    faults.arm(point, FaultAction::Crash, Some(1));
    let crashed = wc.rebase(r.as_str()).await;
    faults.reset();
    if crashed.is_ok() || faults.hits(point) == 0 {
        return Err(expectation(format!("the rebase did not crash at {point}")));
    }
    if !matches!(wc.state().pending, Some(PendingOperation::Rebase { .. })) {
        return Err(expectation("the crash left no pending rebase"));
    }
    let root = wc.root().to_path_buf();
    drop(wc);
    let opts = WcOptions {
        faults,
        ..WcOptions::default()
    };
    let mut wc = WorkingCopy::open(&root, s.client.clone(), opts)?;
    wc.status().await?;
    let outcome = if conflict {
        match &wc.state().pending {
            Some(PendingOperation::Rebase { conflicts, .. })
                if conflicts == &vec![b"rc/h.txt".to_vec()] => {}
            other => {
                return Err(expectation(format!(
                    "the resumed rebase is not paused on the conflict: {other:?}"
                )));
            }
        }
        let markers = scion_merge::render_markers(h0.as_bytes(), h_up.as_bytes(), h1.as_bytes());
        if wc.safe.read(&rp("rc/h.txt")?)? != markers {
            return Err(expectation("the resumed rebase did not write the markers"));
        }
        let resolved = format!("h resolved {n}\n");
        write_files(&wc, &[("rc/h.txt", Mode::File, resolved.as_bytes())])?;
        wc.resolve(&rp("rc/h.txt")?, false)?;
        wc.commit(b"", b"", None).await?;
        Recovered::PausedOnConflicts
    } else {
        Recovered::Completed
    };
    if wc.state().pending.is_some() {
        return Err(expectation("the rebase did not complete"));
    }
    let top = rpc::fetch_changeset(&s.client, &wc.state().base).await?;
    let Some(below_id) = top.parents.first().copied() else {
        return Err(expectation("the rebased head has no parent"));
    };
    let below = rpc::fetch_changeset(&s.client, &below_id).await?;
    if top.change_id != m2.change_id
        || below.change_id != m1.change_id
        || below.parents != vec![onto]
    {
        return Err(expectation(
            "the rebased stack is not m1', m2' on top of onto",
        ));
    }
    let disk = wc_snapshot(&wc)?;
    let get = |p: &str| {
        disk.get(&RepoPath::parse(p.as_bytes()).ok()?)
            .map(|(_, c)| c.clone())
    };
    if get("rc/f.txt") != Some(f1.into_bytes()) || get("rc/g.txt") != Some(g_up.into_bytes()) {
        return Err(expectation("the rebased files are not as expected"));
    }
    if !conflict && get("rc/h.txt") != Some(h1.into_bytes()) {
        return Err(expectation("the rebased files are not as expected"));
    }
    expect_committed_state(s, &mut wc, "rebase recovery").await?;
    Ok(outcome)
}

/// Run [`commit_crash_recovery`] or [`rebase_crash_recovery`] for every write fault point.
pub async fn crash_recovery_all(s: &Scenario, r: &RefName) -> Result<Vec<(String, Recovered)>> {
    let mut out = Vec::new();
    for p in write_points() {
        let outcome = if p.starts_with("wc.commit.") {
            commit_crash_recovery(s, r, p).await?
        } else {
            rebase_crash_recovery(s, r, p).await?
        };
        out.push(((*p).to_owned(), outcome));
    }
    Ok(out)
}

/// Whether an error is the client-side refusal of a commit path (SPEC 11.6): outside the
/// scope (INVALID_ARGUMENT/out_of_scope) or inaccessible (PERMISSION_DENIED).
pub fn is_path_refusal(e: &scion_core::ScionError) -> bool {
    e.subcode() == Some(WorkingCopy::SUBCODE_OUT_OF_SCOPE) || e.code == ErrorCode::PermissionDenied
}
