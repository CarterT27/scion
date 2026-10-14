//! SPEC 11.3 goto (P18).
//!
//! The old and new trees are diffed within scope with identical subtree ids pruned
//! ([`crate::fetch::diff_scope`]); only the differences are applied. Dirty files the diff would
//! touch abort the command unless `force`; other dirty files are preserved untouched. The
//! operation is journaled as `PendingOperation::Goto` before any file changes (fault points
//! `wc.goto.after_journal`, `wc.goto.mid_write`).

use crate::checkout::Existing;
use crate::fetch::Change;
use crate::fetch::{ScopeDiff, diff_scope};
use crate::fs_safe::{case_collisions, check_entry_path};
use crate::index::StatCache;
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

/// The dirty paths a change at `p` touches: the same path, a dirty file where a directory is
/// needed (an ancestor), or dirty files under a directory being replaced (descendants).
/// O(depth + matches) set lookups, so goto stays proportional to the diff.
pub(crate) fn touched_by(p: &RepoPath, dirty: &BTreeSet<RepoPath>) -> Vec<RepoPath> {
    let mut out: Vec<RepoPath> = p
        .ancestors()
        .into_iter()
        .filter(|a| !a.is_root() && dirty.contains(a))
        .collect();
    let pb = p.as_bytes();
    // Every path with `p` as a byte prefix sorts contiguously from `p` itself.
    for d in dirty.range(p.clone()..) {
        let db = d.as_bytes();
        if !db.starts_with(pb) {
            break;
        }
        if db.len() == pb.len() || p.is_root() || db.get(pb.len()) == Some(&b'/') {
            out.push(d.clone());
        }
    }
    out
}

/// Paths the diff would newly create that collide under ASCII case folding with a file that
/// stays (or with an earlier new path) on a case-insensitive filesystem (SPEC 4.4, PLAN 6.17).
/// They are reported and not written. `present` are the files on disk before the goto.
pub(crate) fn case_skips(
    present: &BTreeSet<RepoPath>,
    changes: &[crate::fetch::Change],
) -> (BTreeSet<RepoPath>, Vec<(RepoPath, RepoPath)>) {
    let removed: BTreeSet<&RepoPath> = changes
        .iter()
        .filter(|c| c.new.is_none())
        .map(|c| &c.path)
        .collect();
    let created: BTreeSet<&RepoPath> = changes
        .iter()
        .filter(|c| c.new.is_some() && !present.contains(&c.path))
        .map(|c| &c.path)
        .collect();
    if created.is_empty() {
        return (BTreeSet::new(), Vec::new());
    }
    let mut finals: BTreeSet<RepoPath> = present
        .iter()
        .filter(|p| !removed.contains(p))
        .cloned()
        .collect();
    finals.extend(created.iter().map(|p| (*p).clone()));
    let finals: Vec<RepoPath> = finals.into_iter().collect();
    let mut skip = BTreeSet::new();
    let mut reported = Vec::new();
    for (a, b) in case_collisions(&finals) {
        // Keep what is already on disk; of two new paths keep the first.
        let victim = if created.contains(&b) {
            b.clone()
        } else if created.contains(&a) {
            a.clone()
        } else {
            continue;
        };
        if skip.insert(victim.clone()) {
            if victim == a {
                reported.push((b, a));
            } else {
                reported.push((a, b));
            }
        }
    }
    (skip, reported)
}

/// Untracked files that a path newly created by the diff would overwrite on a
/// case-insensitive filesystem (dirty files in the way, SPEC 4.4, 11.3).
pub(crate) fn untracked_case_conflicts(added: &[RepoPath], changes: &[Change]) -> Vec<RepoPath> {
    let untracked: BTreeSet<&RepoPath> = added.iter().collect();
    let created: BTreeSet<&RepoPath> = changes
        .iter()
        .filter(|c| c.new.is_some() && !untracked.contains(&c.path))
        .map(|c| &c.path)
        .collect();
    if created.is_empty() || untracked.is_empty() {
        return Vec::new();
    }
    let all: Vec<RepoPath> = untracked
        .iter()
        .chain(created.iter())
        .map(|p| (*p).clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut out = BTreeSet::new();
    for (a, b) in case_collisions(&all) {
        if untracked.contains(&a) && created.contains(&b) {
            out.insert(a);
        } else if untracked.contains(&b) && created.contains(&a) {
            out.insert(b);
        }
    }
    out.into_iter().collect()
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
            touched.extend(touched_by(&c.path, &dirty));
        }
        if self.opts.force_case_insensitive || self.safe.is_case_insensitive() {
            touched.extend(untracked_case_conflicts(&status.added, &d.changes));
        }
        if !touched.is_empty() && !force {
            return Err(dirty_error("goto", touched.len()));
        }
        let preserved: Vec<String> = dirty
            .iter()
            .filter(|p| !touched.contains(*p))
            .map(lossy)
            .collect();

        let changes = self.without_case_collisions(&idx, &d.changes);

        if let Some(t) = tracking {
            self.state.tracking_ref = Some(t);
        }
        self.state.pending = Some(PendingOperation::Goto { from, to });
        self.save_state()?;
        self.opts.faults.check(points::GOTO_AFTER_JOURNAL).await?;
        let outcome = self
            .apply_changes(
                &mut idx,
                &changes,
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

    /// Drop (and record in [`WorkingCopy::collisions`]) the writes that would collide under
    /// case folding with a file that stays, when the filesystem is or is forced
    /// case-insensitive (SPEC 4.4: report, never silently overwrite). Computed from the index
    /// of the goto's start, so recovery recomputes the same result. (Untracked files that
    /// collide with a new path are dirty files in the way: see [`untracked_case_conflicts`].)
    fn without_case_collisions(&mut self, idx: &StatCache, changes: &[Change]) -> Vec<Change> {
        self.collisions.clear();
        if !(self.opts.force_case_insensitive || self.safe.is_case_insensitive()) {
            return changes.to_vec();
        }
        let present: BTreeSet<RepoPath> = idx.entries.keys().cloned().collect();
        let (skip, reported) = case_skips(&present, changes);
        if skip.is_empty() {
            return changes.to_vec();
        }
        tracing::warn!(
            count = skip.len(),
            "case collisions: colliding entries are reported and not written"
        );
        self.collisions = reported;
        changes
            .iter()
            .filter(|c| !(c.new.is_some() && skip.contains(&c.path)))
            .cloned()
            .collect()
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
        let changes = self.without_case_collisions(&idx, &d.changes);
        let outcome = self
            .apply_changes(&mut idx, &changes, None, Existing::SkipMatching)
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

    fn change(p: &str, new: bool) -> Change {
        let e = scion_core::EntryInfo {
            mode: scion_core::Mode::File,
            id: ObjectId::hash_blob(p.as_bytes()),
            size: 1,
        };
        Change {
            path: rp(p),
            old: None,
            new: new.then_some(e),
        }
    }

    #[test]
    fn case_skip_rules() {
        let present: BTreeSet<RepoPath> = [rp("readme"), rp("Dir/x"), rp("gone")]
            .into_iter()
            .collect();
        let changes = vec![
            change("README", true), // collides with a staying file: skipped
            change("dir/y", true),  // collides with the staying directory Dir
            change("GONE", true),   // case-only rename: the old file is removed
            change("gone", false),
            change("new", true),
            change("NEW", true),    // two new paths: the first (NEW) is kept
            change("readme", true), // a modification of a present file
        ];
        let (skip, reported) = case_skips(&present, &changes);
        let skip: Vec<RepoPath> = skip.into_iter().collect();
        assert_eq!(skip, vec![rp("README"), rp("dir/y"), rp("new")]);
        assert!(reported.contains(&(rp("readme"), rp("README"))));
        assert!(reported.contains(&(rp("Dir/x"), rp("dir/y"))));
        assert!(reported.contains(&(rp("NEW"), rp("new"))));
        assert!(case_skips(&present, &[change("other", true)]).0.is_empty());

        let untracked = vec![rp("notes.TXT")];
        assert_eq!(
            untracked_case_conflicts(&untracked, &[change("notes.txt", true)]),
            vec![rp("notes.TXT")]
        );
        assert!(untracked_case_conflicts(&untracked, &[change("notes.txt", false)]).is_empty());
    }

    #[test]
    fn touched_by_is_component_wise() {
        let dirty: BTreeSet<RepoPath> = [rp("a"), rp("a!"), rp("a/b/c"), rp("ab"), rp("x/y")]
            .into_iter()
            .collect();
        assert_eq!(touched_by(&rp("a"), &dirty), vec![rp("a"), rp("a/b/c")]);
        assert_eq!(touched_by(&rp("a/b"), &dirty), vec![rp("a"), rp("a/b/c")]);
        assert_eq!(touched_by(&rp("x/y/z"), &dirty), vec![rp("x/y")]);
        assert!(touched_by(&rp("x/yz"), &dirty).is_empty());
    }

    #[test]
    fn touch_rules() {
        let dirty: BTreeSet<RepoPath> = [rp("a/b"), rp("x")].into_iter().collect();
        let touches = |p: &str| !touched_by(&rp(p), &dirty).is_empty();
        assert!(touches("a/b"));
        assert!(touches("a")); // directory replaced by a file
        assert!(touches("x/y")); // file replaced by a directory
        assert!(!touches("a/bc"));
        assert!(!touches("y"));
    }
}
