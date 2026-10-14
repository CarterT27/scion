//! SPEC 11.7 rebase with conflict materialization, continuation, abort and recovery (P28).
//!
//! `rebase(onto)`:
//! 1. lock, recover, refresh redactions; refuse unless the working copy is clean (untracked
//!    files included), because the rebase rewrites the materialization;
//! 2. resolve `onto`; the local stack is the first-parent chain from the base back to (not
//!    including) the first changeset that is an ancestor of `onto` (IsAncestor), oldest first;
//! 3. journal `PendingOperation::Rebase` [`wc.rebase.after_journal`];
//! 4. per member: server `Rebase(member, prev)` where `prev` is the previous result (or
//!    `onto`), chaining; EMPTY_CHANGE drops the member (its change is already in `prev`);
//!    the journal is updated after every member [`wc.rebase.mid_member`];
//! 5. at the end the materialization moves to the last result, which becomes the base.
//!
//! On MERGE_CONFLICT the working copy is moved to `prev` (onto's merged state), the report's
//! clean entries are written, and each conflict is materialized: a text (`content`) conflict
//! as the file with `render_markers(base, ours, theirs)`; any other kind keeps the `ours`
//! version (or nothing when absent) and saves every side under
//! `.scion/conflicts/<path>.{base,ours,theirs}` [`wc.rebase.after_conflict_write`]. The
//! conflicting paths are then recorded in the journal: the rebase is paused. Clean entries
//! outside the sparse scope are kept in `.scion/rebase-clean`. A conflict outside the scope
//! cannot be resolved here, so the rebase is aborted with INVALID_ARGUMENT/out_of_scope.
//!
//! While paused, `resolve` marks paths, and `commit` (every conflict resolved) creates the
//! member's changeset with its original change id and Meta (author, time, message), parent
//! `prev`, then continues with the rest of the stack. `rebase_abort` restores the original
//! changeset exactly (dirty and untracked files of the paused state are discarded).
//!
//! Recovery: a journal with recorded conflicts is a paused rebase, a stable state left as is.
//! Any other Rebase journal is an interrupted run: the working copy is restored to the
//! journaled base and the run resumes (server Rebase is deterministic and idempotent, and
//! every file step converges when repeated), ending completed or paused on conflicts.

mod materialize;

use crate::commit::rpc;
use crate::commit::{
    CommitReport, CommitRequest, REBASE_CLEAN_FILE, delta_of, read_rebase_clean, refresh_index,
    remove_file,
};
use crate::goto::dirty_paths;
use crate::points;
use crate::remote::resolve_rev;
use crate::state::PendingOperation;
use crate::wc::{WorkingCopy, dirty_error};
use scion_core::{EntryInfo, ErrorCode, ErrorPayload, ObjectId, Result, ScionError};
use scion_object::RepoPath;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Directory of conflict side files below `.scion/` (SPEC 11.1).
pub(crate) const CONFLICTS_DIR: &str = "conflicts";
/// Upper bound on the local stack length (a guard against walking unrelated histories).
const MAX_STACK: usize = 100_000;

/// Result of a rebase (`--json` schema `scion.cli.rebase.v1`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct RebaseReport {
    /// Destination (id text).
    pub onto: String,
    /// Rebased results (id text), in stack order.
    pub rebased: Vec<String>,
    /// Conflicting paths (lossy UTF-8), empty when clean.
    pub conflicts: Vec<String>,
    /// New base (id text) when the rebase completed.
    pub head: Option<String>,
}

/// The fields of `PendingOperation::Rebase`.
#[derive(Clone, Debug)]
pub(crate) struct RebaseState {
    pub original: ObjectId,
    pub onto: ObjectId,
    pub remaining: Vec<ObjectId>,
    pub done: Vec<ObjectId>,
    pub conflicts: Vec<Vec<u8>>,
    pub resolved: Vec<Vec<u8>>,
    pub current: Option<ObjectId>,
}

impl RebaseState {
    fn from_pending(p: &Option<PendingOperation>) -> Option<Self> {
        match p {
            Some(PendingOperation::Rebase {
                original,
                onto,
                remaining_stack,
                done,
                conflicts,
                resolved,
                current,
            }) => Some(RebaseState {
                original: *original,
                onto: *onto,
                remaining: remaining_stack.clone(),
                done: done.clone(),
                conflicts: conflicts.clone(),
                resolved: resolved.clone(),
                current: *current,
            }),
            _ => None,
        }
    }

    fn into_pending(self) -> PendingOperation {
        PendingOperation::Rebase {
            original: self.original,
            onto: self.onto,
            remaining_stack: self.remaining,
            done: self.done,
            conflicts: self.conflicts,
            resolved: self.resolved,
            current: self.current,
        }
    }

    /// The changeset the next member is rebased onto.
    pub(crate) fn prev(&self) -> ObjectId {
        self.done.last().copied().unwrap_or(self.onto)
    }

    fn report(&self, conflicts: Vec<String>, head: Option<ObjectId>) -> RebaseReport {
        RebaseReport {
            onto: self.onto.to_text(),
            rebased: self.done.iter().map(|d| d.to_text()).collect(),
            conflicts,
            head: head.map(|h| h.to_text()),
        }
    }
}

fn lossy(p: &[u8]) -> String {
    String::from_utf8_lossy(p).into_owned()
}

fn no_rebase() -> ScionError {
    ScionError::invalid(WorkingCopy::SUBCODE_NO_REBASE, "no rebase is in progress")
}

impl WorkingCopy {
    /// INVALID_ARGUMENT subcode: a commit continuing a rebase found unresolved conflicts
    /// (SPEC 11.7).
    pub const SUBCODE_UNRESOLVED: &'static str = "unresolved_conflicts";

    /// The side file of conflict `p` for `side` (`base`, `ours` or `theirs`):
    /// `.scion/conflicts/<path>.<side>` (SPEC 11.1, 11.7).
    pub fn conflict_side_path(&self, p: &RepoPath, side: &str) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let mut name = p.as_bytes().to_vec();
        name.push(b'.');
        name.extend_from_slice(side.as_bytes());
        self.scion_dir()
            .join(CONFLICTS_DIR)
            .join(std::ffi::OsStr::from_bytes(&name))
    }

    pub(crate) fn rebase_state(&self) -> Option<RebaseState> {
        RebaseState::from_pending(&self.state.pending)
    }

    fn set_rebase_state(&mut self, s: RebaseState) -> Result<()> {
        self.state.pending = Some(s.into_pending());
        self.save_state()
    }

    /// Remove `.scion/conflicts` and `.scion/rebase-clean`.
    pub(crate) fn clear_conflict_files(&self) -> Result<()> {
        let dir = self.scion_dir().join(CONFLICTS_DIR);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(crate::fs_safe::io_error("remove .scion/conflicts", e)),
        }
        remove_file(&self.scion_dir(), REBASE_CLEAN_FILE)
    }

    /// Rebase the local stack onto `onto`; conflicts are materialized and recorded in pending
    /// (SPEC 11.7).
    pub async fn rebase(&mut self, onto: &str) -> Result<RebaseReport> {
        let _lock = self.begin_mutation().await?;
        if self.state.pending.is_some() {
            return Err(ScionError::invalid(
                WorkingCopy::SUBCODE_REBASE_IN_PROGRESS,
                "a rebase is already in progress; resolve and commit, or rebase --abort",
            ));
        }
        let (onto_id, _) = resolve_rev(&self.client, onto).await?;
        let mut idx = self.load_index()?;
        let status = self.status_inner(&mut idx).await?;
        self.save_index(&mut idx)?;
        if !status.is_clean() {
            return Err(dirty_error("rebase", dirty_paths(&status).len()));
        }
        let original = self.state.base;
        let stack = self.local_stack(original, onto_id).await?;
        tracing::info!(members = stack.len(), "rebasing the local stack");
        self.clear_conflict_files()?;
        self.set_rebase_state(RebaseState {
            original,
            onto: onto_id,
            remaining: stack,
            done: Vec::new(),
            conflicts: Vec::new(),
            resolved: Vec::new(),
            current: None,
        })?;
        self.opts.faults.check(points::REBASE_AFTER_JOURNAL).await?;
        self.run_rebase().await
    }

    /// The first-parent chain from `base` back to the first ancestor of `onto`, oldest first.
    async fn local_stack(&self, base: ObjectId, onto: ObjectId) -> Result<Vec<ObjectId>> {
        let mut stack = Vec::new();
        let mut m = base;
        while !rpc::is_ancestor(&self.client, &m, &onto).await? {
            if stack.len() >= MAX_STACK {
                return Err(ScionError::invalid(
                    scion_core::subcode::INVALID_REVISION,
                    "the local stack is too long; is the destination related to the base?",
                ));
            }
            let cs = rpc::fetch_changeset(&self.client, &m).await?;
            stack.push(m);
            match cs.parents.first() {
                Some(p) => m = *p,
                None => {
                    return Err(ScionError::invalid(
                        scion_core::subcode::INVALID_REVISION,
                        "the base and the destination have no common ancestor",
                    ));
                }
            }
        }
        stack.reverse();
        Ok(stack)
    }

    /// Process the journaled stack from where it stands: rebase the remaining members, then
    /// move to the result; stops (paused) at the first conflict.
    pub(crate) async fn run_rebase(&mut self) -> Result<RebaseReport> {
        loop {
            let Some(mut st) = self.rebase_state() else {
                return Err(no_rebase());
            };
            let prev = st.prev();
            let Some(m) = st.remaining.first().copied() else {
                let mut idx = self.load_index()?;
                self.move_base(prev, &mut idx).await?;
                self.state.pending = None;
                self.save_state()?;
                self.save_index(&mut idx)?;
                self.clear_conflict_files()?;
                tracing::info!(head = %prev, "rebase completed");
                return Ok(st.report(Vec::new(), Some(prev)));
            };
            if st.current != Some(m) {
                st.current = Some(m);
                self.set_rebase_state(st.clone())?;
            }
            match rpc::rebase(&self.client, &m, &prev).await {
                Ok(r) => {
                    st.done.push(r);
                    st.remaining.remove(0);
                    st.current = None;
                    self.set_rebase_state(st)?;
                }
                Err(e) if e.code == ErrorCode::EmptyChange => {
                    tracing::info!(member = %m, "dropping a stack member already contained in the destination");
                    st.remaining.remove(0);
                    st.current = None;
                    self.set_rebase_state(st)?;
                }
                Err(e) if e.code == ErrorCode::MergeConflict => {
                    let report = match e.payload {
                        ErrorPayload::MergeConflict(r) => *r,
                        _ => Default::default(),
                    };
                    return self.materialize_conflicts(st, m, prev, report).await;
                }
                Err(e) => return Err(e),
            }
            self.opts.faults.check(points::REBASE_MID_MEMBER).await?;
        }
    }

    /// Return to the original changeset and clear pending (SPEC 11.7 `rebase --abort`).
    pub async fn rebase_abort(&mut self) -> Result<()> {
        let _l = self.lock()?;
        if self.rebase_state().is_none() {
            if self.state.pending.is_some() {
                self.recover_inner().await?;
            }
            return Err(no_rebase());
        }
        self.abort_to_original().await
    }

    /// Restore the dirty files to the base, move to the original changeset, clear the
    /// journal and the conflict files.
    pub(crate) async fn abort_to_original(&mut self) -> Result<()> {
        let Some(st) = self.rebase_state() else {
            return Err(no_rebase());
        };
        let mut idx = self.load_index()?;
        self.reset_hard(&mut idx).await?;
        self.move_base(st.original, &mut idx).await?;
        self.state.pending = None;
        self.save_state()?;
        self.save_index(&mut idx)?;
        self.clear_conflict_files()?;
        tracing::info!(original = %st.original, "rebase aborted");
        Ok(())
    }

    /// `commit` while a rebase is paused on conflicts (SPEC 11.7): every conflict must be
    /// resolved; the member's changeset is created with its original change id and Meta on
    /// top of `prev`, then the rest of the stack is processed.
    pub(crate) async fn continue_rebase(&mut self) -> Result<CommitReport> {
        let Some(mut st) = self.rebase_state() else {
            return Err(no_rebase());
        };
        if st.conflicts.is_empty() {
            // An interrupted run that recovery could not finish: nothing to commit yet.
            return Err(ScionError::invalid(
                WorkingCopy::SUBCODE_REBASE_IN_PROGRESS,
                "the rebase is not paused on conflicts",
            ));
        }
        let unresolved = st
            .conflicts
            .iter()
            .filter(|c| !st.resolved.contains(c))
            .count();
        if unresolved > 0 {
            return Err(ScionError::invalid(
                WorkingCopy::SUBCODE_UNRESOLVED,
                format!("{unresolved} conflicting path(s) are not resolved"),
            ));
        }
        let Some(member) = st.current.or_else(|| st.remaining.first().copied()) else {
            return Err(no_rebase());
        };
        let prev = st.prev();
        if self.state.base != prev {
            return Err(ScionError::new(
                ErrorCode::InvalidArgument,
                "the working copy is not at the paused rebase step; run rebase --abort",
            ));
        }
        let info = rpc::fetch_changeset(&self.client, &member).await?;
        let meta = rpc::meta_fields_of(self.fetch_meta(&info.meta).await?);
        let mut idx = self.load_index()?;
        let changes = self.working_changes(&mut idx).await?;
        self.save_index(&mut idx)?;
        let mut delta: BTreeMap<RepoPath, Option<EntryInfo>> =
            changes.iter().map(|c| (c.path.clone(), c.entry)).collect();
        let outside = read_rebase_clean(&self.scion_dir())?;
        if !outside.is_empty() {
            let prev_root = self.root_tree(&prev).await?;
            for (p, e) in outside {
                if delta.contains_key(&p) {
                    continue;
                }
                if self.entry_at(&prev_root, &p).await? != e {
                    delta.insert(p, e);
                }
            }
        }
        let mut report = CommitReport {
            changeset: prev,
            root_tree: self.root_tree(&prev).await?,
            change_id: info.change_id,
            parents: vec![prev],
            delta: delta_of(&delta),
            uploaded: 0,
        };
        if delta.is_empty() {
            tracing::info!(member = %member, "the resolved member is empty; dropping it");
        } else {
            let req = CommitRequest {
                parents: vec![prev],
                delta: report.delta.clone(),
                meta,
                change_id: info.change_id,
            };
            report.uploaded = self.upload_missing(&changes).await?;
            self.opts.faults.check(points::COMMIT_AFTER_UPLOAD).await?;
            let created = rpc::create_changeset(&self.client, &req).await?;
            self.opts.faults.check(points::COMMIT_AFTER_CREATE).await?;
            self.cache
                .set_root(&created.changeset, &created.root_tree)?;
            self.state.base = created.changeset;
            st.done.push(created.changeset);
            report.changeset = created.changeset;
            report.root_tree = created.root_tree;
            refresh_index(&mut idx, &changes);
        }
        if st.remaining.first() == Some(&member) {
            st.remaining.remove(0);
        }
        st.current = None;
        st.conflicts.clear();
        st.resolved.clear();
        self.set_rebase_state(st)?;
        self.save_index(&mut idx)?;
        self.clear_conflict_files()?;
        self.run_rebase().await?;
        Ok(report)
    }
}

/// Resume or roll back a pending Rebase (called by `WorkingCopy::recover`; SPEC 11.3, 11.7,
/// 16.1 class 8). A rebase paused on conflicts is a stable state and is left alone; an
/// interrupted run is restored to its journaled base and resumed.
pub(crate) async fn recover_rebase(wc: &mut WorkingCopy) -> Result<Option<String>> {
    let Some(st) = wc.rebase_state() else {
        return Ok(None);
    };
    if !st.conflicts.is_empty() {
        return Ok(None);
    }
    let mut idx = wc.load_index()?;
    let restored = wc.reset_hard(&mut idx).await?;
    wc.save_index(&mut idx)?;
    let report = wc.run_rebase().await?;
    tracing::info!(restored, "resumed an interrupted rebase");
    Ok(Some(if report.conflicts.is_empty() {
        format!(
            "completed interrupted rebase onto {}",
            report.head.unwrap_or_default()
        )
    } else {
        format!(
            "resumed interrupted rebase; {} conflicting path(s) to resolve",
            report.conflicts.len()
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::Kind;

    #[test]
    fn state_round_trip_and_prev() {
        let id = |b: u8| ObjectId::new(Kind::Changeset, [b; 32]);
        let st = RebaseState {
            original: id(1),
            onto: id(2),
            remaining: vec![id(3)],
            done: vec![],
            conflicts: vec![b"a".to_vec()],
            resolved: vec![],
            current: Some(id(3)),
        };
        assert_eq!(st.prev(), id(2));
        let back = RebaseState::from_pending(&Some(st.clone().into_pending())).unwrap();
        assert_eq!(back.remaining, st.remaining);
        assert_eq!(back.current, st.current);
        let mut st2 = back;
        st2.done.push(id(9));
        assert_eq!(st2.prev(), id(9));
        assert!(RebaseState::from_pending(&None).is_none());
        assert_eq!(lossy(b"a\xff"), "a\u{fffd}");
    }
}
