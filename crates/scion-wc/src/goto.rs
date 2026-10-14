//! SPEC 11.3 goto (P18).
//!
//! The old and new trees are diffed within scope with identical subtree ids pruned
//! ([`crate::fetch::diff_scope`]); only the differences are applied. Dirty files the diff would
//! touch abort the command unless `force`; other dirty files are preserved untouched. The
//! operation is journaled as `PendingOperation::Goto` before any file changes (fault points
//! `wc.goto.after_journal`, `wc.goto.mid_write`).

use crate::checkout::Existing;
use crate::fetch::{ScopeDiff, diff_scope};
use crate::fs_safe::check_entry_path;
use crate::points;
use crate::remote::resolve_rev;
use crate::state::PendingOperation;
use crate::status::StatusReport;
use crate::wc::{WorkingCopy, dirty_error};
use scion_core::{ObjectId, Result};
use scion_object::RepoPath;
use std::collections::BTreeSet;

/// Result of a goto (`--json` schema `scion.cli.goto.v1`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct GotoReport {
    /// Previous base (id text).
    pub from: String,
    /// New base (id text).
    pub to: String,
    /// Files written.
    pub written: u64,
    /// Files removed.
    pub removed: u64,
    /// Dirty files preserved untouched.
    pub preserved_dirty: Vec<String>,
}

/// Every dirty path of a status report (SPEC 11.5).
pub(crate) fn dirty_paths(r: &StatusReport) -> BTreeSet<RepoPath> {
    r.modified
        .iter()
        .chain(&r.added)
        .chain(&r.deleted)
        .chain(&r.mode_changed)
        .chain(&r.redacted_dirty)
        .cloned()
        .collect()
}

/// `a` equals `b` or is a component-wise ancestor of it.
pub(crate) fn covers(a: &RepoPath, b: &RepoPath) -> bool {
    a == b || a.is_ancestor_of(b)
}

/// Whether a change at `p` touches a dirty path: the same path, a dirty file where a directory
/// is needed (an ancestor), or dirty files under a directory being replaced (descendants).
pub(crate) fn touches(p: &RepoPath, dirty: &BTreeSet<RepoPath>) -> bool {
    dirty.iter().any(|d| covers(p, d) || d.is_ancestor_of(p))
}

/// Inaccessible prefixes after a diff: old entries whose subtree was not re-examined are kept;
/// newly denied subtrees and blobs are added (SPEC 12.6).
pub(crate) fn updated_inaccessible(
    old: &[RepoPath],
    d: &ScopeDiff,
    denied_blobs: &[RepoPath],
) -> Vec<RepoPath> {
    let changed: BTreeSet<&RepoPath> = d.changes.iter().map(|c| &c.path).collect();
    let mut out: BTreeSet<RepoPath> = old
        .iter()
        .filter(|p| !d.visited.contains(*p) && !changed.contains(p))
        .cloned()
        .collect();
    out.extend(d.denied.iter().cloned());
    out.extend(denied_blobs.iter().cloned());
    out.into_iter().collect()
}

fn lossy(p: &RepoPath) -> String {
    String::from_utf8_lossy(p.as_bytes()).into_owned()
}

impl WorkingCopy {
    /// Move to `rev`, applying only the pruned diff within scope; dirty files the diff would
    /// touch abort unless `force` (SPEC 11.3).
    pub async fn goto(&mut self, rev: &str, force: bool) -> Result<GotoReport> {
        let _lock = self.begin_mutation().await?;
        let (to, tracking) = resolve_rev(&self.client, rev).await?;
        let from = self.state.base;
        let from_root = self.root_tree(&from).await?;
        let to_root = self.root_tree(&to).await?;
        let mut idx = self.load_index()?;
        let status = self.status_inner(&mut idx).await?;
        let d = diff_scope(
            &self.fetcher,
            &self.cache,
            &from_root,
            &to_root,
            &self.state.sparse,
            &self.state.inaccessible,
        )
        .await?;
        for c in d.changes.iter().filter(|c| c.new.is_some()) {
            check_entry_path(c.path.as_bytes())?;
        }
        let dirty = dirty_paths(&status);
        let deleted: BTreeSet<&RepoPath> = status.deleted.iter().collect();
        let mut touched: BTreeSet<RepoPath> = BTreeSet::new();
        for c in &d.changes {
            // A locally deleted file that the target deletes too is no conflict.
            if c.new.is_none() && deleted.contains(&c.path) {
                continue;
            }
            if touches(&c.path, &dirty) {
                for p in dirty
                    .iter()
                    .filter(|p| covers(&c.path, p) || p.is_ancestor_of(&c.path))
                {
                    touched.insert(p.clone());
                }
            }
        }
        if !touched.is_empty() && !force {
            return Err(dirty_error("goto", touched.len()));
        }
        let preserved: Vec<String> = dirty
            .iter()
            .filter(|p| !touched.contains(*p))
            .map(lossy)
            .collect();

        if let Some(t) = tracking {
            self.state.tracking_ref = Some(t);
        }
        self.state.pending = Some(PendingOperation::Goto { from, to });
        self.save_state()?;
        self.opts.faults.check(points::GOTO_AFTER_JOURNAL).await?;
        let outcome = self
            .apply_changes(
                &mut idx,
                &d.changes,
                Some(points::GOTO_MID_WRITE),
                Existing::Overwrite,
            )
            .await?;
        self.finish_goto(to, &d, &outcome.denied)?;
        self.save_index(&mut idx)?;
        Ok(GotoReport {
            from: from.to_text(),
            to: to.to_text(),
            written: outcome.written,
            removed: outcome.removed,
            preserved_dirty: preserved,
        })
    }

    /// Commit the end of a goto to the state: new base, inaccessible prefixes, no pending.
    pub(crate) fn finish_goto(
        &mut self,
        to: ObjectId,
        d: &ScopeDiff,
        denied_blobs: &[RepoPath],
    ) -> Result<()> {
        self.state.inaccessible = updated_inaccessible(&self.state.inaccessible, d, denied_blobs);
        self.state.base = to;
        self.state.pending = None;
        self.save_state()
    }

    /// Roll an interrupted goto forward (SPEC 11.3): re-apply the diff, skipping files that
    /// already hold their target content.
    pub(crate) async fn complete_goto(&mut self, from: ObjectId, to: ObjectId) -> Result<()> {
        let from_root = self.root_tree(&from).await?;
        let to_root = self.root_tree(&to).await?;
        let d = diff_scope(
            &self.fetcher,
            &self.cache,
            &from_root,
            &to_root,
            &self.state.sparse,
            &self.state.inaccessible,
        )
        .await?;
        let mut idx = self.load_index()?;
        let outcome = self
            .apply_changes(&mut idx, &d.changes, None, Existing::SkipMatching)
            .await?;
        self.finish_goto(to, &d, &outcome.denied)?;
        self.save_index(&mut idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rp(s: &str) -> RepoPath {
        RepoPath::parse(s.as_bytes()).unwrap()
    }

    #[test]
    fn touch_rules() {
        let dirty: BTreeSet<RepoPath> = [rp("a/b"), rp("x")].into_iter().collect();
        assert!(touches(&rp("a/b"), &dirty));
        assert!(touches(&rp("a"), &dirty)); // directory replaced by a file
        assert!(touches(&rp("x/y"), &dirty)); // file replaced by a directory
        assert!(!touches(&rp("a/bc"), &dirty));
        assert!(!touches(&rp("y"), &dirty));
    }
}
