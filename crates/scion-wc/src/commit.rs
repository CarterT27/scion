//! SPEC 11.6 commit and amend, and recovery of an interrupted commit (SPEC 11.3, 16.1
//! class 8; P28).
//!
//! Commit order (fault points in brackets):
//! 1. lock, complete or roll back any pending operation, refresh redactions (SPEC 11.3, 13.7);
//! 2. status within scope; `paths` restricts it and must lie inside the scope and outside
//!    inaccessible prefixes (SPEC 11.6, 12.6);
//! 3. the delta against the base (Put with mode, blob id and size; Delete), sorted by path;
//! 4. HasObjects for its blobs;
//! 5. write the CreateChangeset request to `.scion/commit-request` and journal
//!    `PendingOperation::Commit` [`wc.commit.after_journal`];
//! 6. UploadObjects of only the missing blobs [`wc.commit.after_upload`];
//! 7. CreateChangeset(parents = [base], new random change id, Meta with the author and the
//!    current time in the local time zone) [`wc.commit.after_create`];
//! 8. move the base to the result, refresh the index, clear the journal.
//!
//! A definitive failure (a non-retryable error) rolls the journal back at once; a retryable
//! one (including an injected crash) leaves it for recovery. [`recover_commit`] re-issues the
//! journaled request, which is deterministic and idempotent: when it succeeds the changeset
//! exists (now or before) and the commit is finished by moving the base; when it fails
//! definitively (for example a blob was never uploaded) the commit is rolled back by clearing
//! the journal, leaving the files untouched, so the changes show up in status again.
//!
//! Amend replaces the base: CreateChangeset(parents = base.parents, same change id), with the
//! delta recomputed against the first parent (the base's own delta composed with the working
//! changes) and the original Meta fields, except the message when a new one is given.
//!
//! Commit while a rebase is paused on conflicts continues the rebase (see `rebase.rs`).

mod journal;
pub(crate) mod rpc;
mod tz;
mod work;

pub(crate) use journal::{
    COMMIT_REQUEST_FILE, CommitRequest, REBASE_CLEAN_FILE, entry_of, op_of, read_rebase_clean,
    remove_file, write_rebase_clean,
};
pub(crate) use work::WorkChange;

use crate::index::{IndexEntry, StatCache};
use crate::points;
use crate::state::PendingOperation;
use crate::status::Prefixes;
use crate::wc::WorkingCopy;
use scion_core::{
    ChangeId, Clock, EntryInfo, ErrorCode, ObjectId, Result, ScionError, SystemClock,
};
use scion_object::{DeltaEntry, Extensions, RepoPath, Timestamp};
use scion_proto::convert::MetaFieldsParts;
use std::collections::BTreeMap;
use std::time::UNIX_EPOCH;

/// Result of a commit or amend (SPEC 11.6, 19.10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitReport {
    /// The created changeset.
    pub changeset: ObjectId,
    /// Its root tree.
    pub root_tree: ObjectId,
    /// Its change id (new for a commit, preserved by amend and rebase continuation).
    pub change_id: ChangeId,
    /// Its parents.
    pub parents: Vec<ObjectId>,
    /// The delta sent in CreateChangeset, ascending by path.
    pub delta: Vec<DeltaEntry>,
    /// Blobs sent through UploadObjects (only those HasObjects reported missing).
    pub uploaded: usize,
}

/// The current time with the local UTC offset (SPEC 4.3.3), from the system clock.
pub(crate) fn now_timestamp() -> Timestamp {
    let now = SystemClock.now();
    let seconds = match now.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    };
    Timestamp {
        seconds,
        tz_offset_minutes: tz::local_offset_minutes(seconds),
    }
}

/// Sorted delta entries of a path -> entry map.
pub(crate) fn delta_of(m: &BTreeMap<RepoPath, Option<EntryInfo>>) -> Vec<DeltaEntry> {
    m.iter()
        .map(|(p, e)| DeltaEntry {
            path: p.clone(),
            op: op_of(*e),
        })
        .collect()
}

fn rebase_in_progress(what: &str) -> ScionError {
    ScionError::invalid(
        WorkingCopy::SUBCODE_REBASE_IN_PROGRESS,
        format!("{what}: a rebase is in progress; resolve and commit, or rebase --abort"),
    )
}

impl WorkingCopy {
    /// INVALID_ARGUMENT subcode: a commit path lies outside the sparse scope (SPEC 11.6).
    pub const SUBCODE_OUT_OF_SCOPE: &'static str = "out_of_scope";
    /// INVALID_ARGUMENT subcode: the command is not allowed while a rebase is paused on
    /// conflicts (SPEC 11.7).
    pub const SUBCODE_REBASE_IN_PROGRESS: &'static str = "rebase_in_progress";

    /// Commit the working-copy changes (optionally only `paths`) on top of the base
    /// (SPEC 11.6). While a rebase is paused on conflicts this continues it (SPEC 11.7).
    pub async fn commit(
        &mut self,
        message: &[u8],
        author: &[u8],
        paths: Option<&[RepoPath]>,
    ) -> Result<ObjectId> {
        Ok(self
            .commit_with_report(message, author, paths)
            .await?
            .changeset)
    }

    /// [`WorkingCopy::commit`] returning the delta and upload statistics (SPEC 11.6, 19.10).
    /// With nothing to commit the result is EMPTY_CHANGE. While a rebase is paused on
    /// conflicts, `message` and `author` are ignored: the continuation keeps the original
    /// change id, author and message (SPEC 11.7).
    pub async fn commit_with_report(
        &mut self,
        message: &[u8],
        author: &[u8],
        paths: Option<&[RepoPath]>,
    ) -> Result<CommitReport> {
        let _lock = self.begin_mutation().await?;
        if matches!(self.state.pending, Some(PendingOperation::Rebase { .. })) {
            if paths.is_some() {
                return Err(ScionError::invalid(
                    WorkingCopy::SUBCODE_REBASE_IN_PROGRESS,
                    "a commit that continues a rebase takes every change; paths are not allowed",
                ));
            }
            return self.continue_rebase().await;
        }
        if let Some(p) = paths {
            self.check_commit_paths(p)?;
        }
        let mut idx = self.load_index()?;
        let mut changes = self.working_changes(&mut idx).await?;
        self.save_index(&mut idx)?;
        if let Some(p) = paths {
            let sel = Prefixes::new(p, &[]);
            changes.retain(|c| sel.in_scope(c.path.as_bytes()));
        }
        if changes.is_empty() {
            return Err(ScionError::new(ErrorCode::EmptyChange, "nothing to commit"));
        }
        let delta: BTreeMap<RepoPath, Option<EntryInfo>> =
            changes.iter().map(|c| (c.path.clone(), c.entry)).collect();
        let req = CommitRequest {
            parents: vec![self.state.base],
            delta: delta_of(&delta),
            meta: MetaFieldsParts {
                author: author.to_vec(),
                author_time: now_timestamp(),
                committer: None,
                committer_time: None,
                message: message.to_vec(),
                extensions: Extensions::new(),
            },
            change_id: ChangeId::random(),
        };
        self.run_commit(req, &changes, &mut idx, false).await
    }

    /// Amend the base changeset (same change id), optionally with a new message (SPEC 11.6).
    pub async fn amend(&mut self, message: Option<&[u8]>) -> Result<ObjectId> {
        Ok(self.amend_with_report(message).await?.changeset)
    }

    /// [`WorkingCopy::amend`] returning the delta and upload statistics.
    pub async fn amend_with_report(&mut self, message: Option<&[u8]>) -> Result<CommitReport> {
        let _lock = self.begin_mutation().await?;
        if self.state.pending.is_some() {
            return Err(rebase_in_progress("amend"));
        }
        let base = self.state.base;
        let cs = rpc::fetch_changeset(&self.client, &base).await?;
        if cs.filtered {
            return Err(ScionError::permission_denied(
                "amend needs every path of the base changeset to be readable",
            ));
        }
        let Some(first) = cs.parents.first().copied() else {
            return Err(ScionError::invalid(
                scion_core::subcode::PARENTLESS_NOT_ALLOWED,
                "cannot amend a parentless changeset",
            ));
        };
        let mut meta = rpc::meta_fields_of(self.fetch_meta(&cs.meta).await?);
        if let Some(m) = message {
            meta.message = m.to_vec();
        }
        let mut idx = self.load_index()?;
        let changes = self.working_changes(&mut idx).await?;
        self.save_index(&mut idx)?;
        // Compose the base's delta (against its first parent) with the working changes.
        let mut delta: BTreeMap<RepoPath, Option<EntryInfo>> = cs
            .delta
            .iter()
            .map(|d| (d.path.clone(), entry_of(&d.op)))
            .collect();
        let first_root = self.root_tree(&first).await?;
        for c in &changes {
            if delta.contains_key(&c.path) {
                let in_parent = self.entry_at(&first_root, &c.path).await?;
                if c.entry == in_parent {
                    delta.remove(&c.path);
                } else {
                    delta.insert(c.path.clone(), c.entry);
                }
            } else {
                delta.insert(c.path.clone(), c.entry);
            }
        }
        let req = CommitRequest {
            parents: cs.parents.clone(),
            delta: delta_of(&delta),
            meta,
            change_id: cs.change_id,
        };
        self.run_commit(req, &changes, &mut idx, true).await
    }

    /// Refuse commit paths outside the scope (INVALID_ARGUMENT/out_of_scope) or at/under an
    /// inaccessible prefix (PERMISSION_DENIED) (SPEC 11.6, 12.6). A directory that leads to
    /// the scope is accepted: only its in-scope part can be committed.
    fn check_commit_paths(&self, paths: &[RepoPath]) -> Result<()> {
        let scope = Prefixes::new(&self.state.sparse, &self.state.inaccessible);
        for p in paths {
            let b = p.as_bytes();
            if !scope.in_scope(b) && !scope.leads_to_scope(b) {
                return Err(ScionError::invalid(
                    WorkingCopy::SUBCODE_OUT_OF_SCOPE,
                    "a commit path lies outside the sparse scope",
                ));
            }
            if scope.inaccessible(b) {
                return Err(ScionError::permission_denied(
                    "a commit path lies in an inaccessible subtree",
                ));
            }
        }
        Ok(())
    }

    /// Steps 4-8 of a commit or amend (journal, upload, create, finish).
    async fn run_commit(
        &mut self,
        req: CommitRequest,
        changes: &[WorkChange],
        idx: &mut StatCache,
        amend: bool,
    ) -> Result<CommitReport> {
        let parent = self.state.base;
        journal::write_commit_request(&self.scion_dir(), &req)?;
        self.state.pending = Some(PendingOperation::Commit {
            parent,
            change_id: req.change_id,
            amend,
        });
        self.save_state()?;
        let result = async {
            self.opts.faults.check(points::COMMIT_AFTER_JOURNAL).await?;
            let uploaded = self.upload_missing(changes).await?;
            self.opts.faults.check(points::COMMIT_AFTER_UPLOAD).await?;
            rpc::create_changeset(&self.client, &req)
                .await
                .map(|c| (uploaded, c))
        }
        .await;
        let (uploaded, created) = match result {
            Ok(v) => v,
            Err(e) => {
                if !e.retryable() {
                    // Definitive: the changeset was not created; nothing to recover.
                    self.rollback_commit()?;
                }
                return Err(e);
            }
        };
        // From here on the changeset exists: a failure leaves the journal, and recovery
        // finishes the commit.
        self.opts.faults.check(points::COMMIT_AFTER_CREATE).await?;
        self.finish_commit(&created)?;
        refresh_index(idx, changes);
        self.save_index(idx)?;
        tracing::info!(changeset = %created.changeset, amend, uploaded, "committed");
        Ok(CommitReport {
            changeset: created.changeset,
            root_tree: created.root_tree,
            change_id: req.change_id,
            parents: req.parents,
            delta: req.delta,
            uploaded,
        })
    }

    /// Move the base to a created changeset and clear the commit journal.
    fn finish_commit(&mut self, created: &rpc::Created) -> Result<()> {
        self.cache
            .set_root(&created.changeset, &created.root_tree)?;
        self.state.base = created.changeset;
        self.state.pending = None;
        self.save_state()?;
        journal::remove_file(&self.scion_dir(), COMMIT_REQUEST_FILE)
    }

    /// Clear a commit journal whose changeset was not created (files untouched).
    fn rollback_commit(&mut self) -> Result<()> {
        self.state.pending = None;
        self.save_state()?;
        journal::remove_file(&self.scion_dir(), COMMIT_REQUEST_FILE)
    }
}

/// Record committed files in the index with the stat taken before they were read, so a file
/// modified since is re-hashed by the next status (SPEC 11.5).
pub(crate) fn refresh_index(idx: &mut StatCache, changes: &[WorkChange]) {
    for c in changes {
        match (&c.entry, &c.stat) {
            (Some(e), Some(st)) => {
                idx.entries.insert(
                    c.path.clone(),
                    IndexEntry {
                        stat: st.clone(),
                        id: e.id,
                        mode: e.mode,
                        redacted: false,
                    },
                );
            }
            _ => {
                idx.entries.remove(&c.path);
            }
        }
    }
}

/// Finish or roll back a pending Commit (called by `WorkingCopy::recover`; SPEC 11.3,
/// 16.1 class 8). The journaled request is re-issued: success means the changeset exists and
/// the base moves to it; a definitive failure means it was never created and the journal is
/// cleared with the files untouched. Retryable failures leave the journal in place.
pub(crate) async fn recover_commit(wc: &mut WorkingCopy) -> Result<Option<String>> {
    let Some(PendingOperation::Commit {
        parent, change_id, ..
    }) = wc.state.pending.clone()
    else {
        return Ok(None);
    };
    let req = journal::read_commit_request(&wc.scion_dir())?;
    let req = match req {
        Some(r) if r.change_id == change_id && wc.state.base == parent => r,
        _ => {
            wc.rollback_commit()?;
            tracing::warn!("rolled back an interrupted commit without a usable journal");
            return Ok(Some(
                "rolled back interrupted commit (no usable journal)".to_owned(),
            ));
        }
    };
    match rpc::create_changeset(&wc.client, &req).await {
        Ok(created) => {
            wc.finish_commit(&created)?;
            tracing::info!(changeset = %created.changeset, "completed an interrupted commit");
            Ok(Some(format!(
                "completed interrupted commit {}",
                created.changeset
            )))
        }
        Err(e) if e.retryable() => Err(e),
        Err(e) => {
            wc.rollback_commit()?;
            tracing::info!(error = %e, "rolled back an interrupted commit");
            Ok(Some(format!(
                "rolled back interrupted commit (changeset not created: {})",
                e.code.as_str()
            )))
        }
    }
}
