//! Working-copy helpers shared by commit, amend, rebase and land (SPEC 11.5-11.7; P28).
//!
//! - [`WorkingCopy::working_changes`]: the files that differ from the base within scope, as
//!   delta entries (status first, then one read per changed file).
//! - [`WorkingCopy::upload_missing`]: HasObjects, then UploadObjects of only the missing blobs,
//!   re-reading each file and checking it still hashes to the committed id.
//! - [`WorkingCopy::entry_at`]: one path of any tree, fetching only the trees on the way.
//! - [`WorkingCopy::move_base`]: move the materialization from the base to another changeset
//!   by applying the pruned scope diff (no journal of its own: callers journal).
//! - [`WorkingCopy::reset_hard`]: restore every dirty in-scope file to the base (untracked
//!   files are removed), used before rebase steps and by `rebase --abort`.

use super::rpc;
use crate::checkout::Existing;
use crate::fetch::{Change, diff_scope, ensure_trees};
use crate::fs_safe::check_entry_path;
use crate::goto::{dirty_paths, updated_inaccessible};
use crate::index::{StatCache, StatTuple};
use crate::remote::Item;
use crate::wc::WorkingCopy;
use scion_core::{EntryInfo, ErrorCode, Mode, ObjectId, Result, ScionError};
use scion_object::{Canonical, Meta, RepoPath};
use std::collections::{BTreeMap, BTreeSet};

/// One working-copy change against the base (SPEC 11.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkChange {
    pub path: RepoPath,
    /// The new entry; `None` deletes the path.
    pub entry: Option<EntryInfo>,
    /// The stat taken before the content was read (index refresh after the commit).
    pub stat: Option<StatTuple>,
}

fn changed_during_commit() -> ScionError {
    ScionError::new(
        ErrorCode::Unavailable,
        "a working-copy file changed while it was being committed; retry",
    )
}

impl WorkingCopy {
    /// The disk state of `p` as an entry plus the stat taken before reading it; `None` when
    /// absent or not a working-copy file (directory, special file).
    pub(crate) fn read_disk_entry(&self, p: &RepoPath) -> Result<Option<(EntryInfo, StatTuple)>> {
        let Some(st) = self.safe.stat(p)? else {
            return Ok(None);
        };
        let Some(mode) = st.entry_mode() else {
            return Ok(None);
        };
        let content = match mode {
            Mode::Symlink => self.safe.read_link(p),
            _ => self.safe.read(p),
        };
        match content {
            Ok(c) => Ok(Some((
                EntryInfo {
                    mode,
                    id: ObjectId::hash_blob(&c),
                    size: c.len() as u64,
                },
                st,
            ))),
            Err(e) if e.code == ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Every in-scope difference between the disk and the base, sorted by path (SPEC 11.5,
    /// 11.6: no staging area). Ignored files, `.scion`, out-of-scope and inaccessible paths
    /// never appear (the status walk skips them). `idx` is refreshed, not saved.
    pub(crate) async fn working_changes(&mut self, idx: &mut StatCache) -> Result<Vec<WorkChange>> {
        let report = self.status_inner(idx).await?;
        let deleted: BTreeSet<&RepoPath> = report.deleted.iter().collect();
        let added: BTreeSet<&RepoPath> = report.added.iter().collect();
        let mut out = Vec::new();
        for p in dirty_paths(&report) {
            check_entry_path(p.as_bytes())?;
            if deleted.contains(&p) {
                out.push(WorkChange {
                    path: p,
                    entry: None,
                    stat: None,
                });
                continue;
            }
            match self.read_disk_entry(&p)? {
                Some((e, st)) => out.push(WorkChange {
                    path: p,
                    entry: Some(e),
                    stat: Some(st),
                }),
                // Vanished since the status walk: a base file is now deleted; an untracked
                // file simply is not there any more.
                None if added.contains(&p) => {}
                None => out.push(WorkChange {
                    path: p,
                    entry: None,
                    stat: None,
                }),
            }
        }
        Ok(out)
    }

    /// The content of the committed file `p` for upload; it must still hash to `id`.
    fn read_for_upload(&self, p: &RepoPath, id: &ObjectId) -> Result<Vec<u8>> {
        let Some(st) = self.safe.stat(p)? else {
            return Err(changed_during_commit());
        };
        let content = match st.entry_mode() {
            Some(Mode::Symlink) => self.safe.read_link(p)?,
            Some(_) => self.safe.read(p)?,
            None => return Err(changed_during_commit()),
        };
        if ObjectId::hash_blob(&content) != *id {
            return Err(changed_during_commit());
        }
        Ok(content)
    }

    /// SPEC 11.6: HasObjects for the blobs of `changes`, then UploadObjects of only the missing
    /// ones (each distinct blob once). Returns the number of uploaded items.
    pub(crate) async fn upload_missing(&self, changes: &[WorkChange]) -> Result<usize> {
        let mut by_id: BTreeMap<ObjectId, &RepoPath> = BTreeMap::new();
        for c in changes {
            if let Some(e) = &c.entry {
                by_id.entry(e.id).or_insert(&c.path);
            }
        }
        if by_id.is_empty() {
            return Ok(0);
        }
        let ids: Vec<ObjectId> = by_id.keys().copied().collect();
        let present = rpc::has_objects(&self.client, &ids).await?;
        let mut blobs = Vec::new();
        for (id, here) in ids.iter().zip(present) {
            if here {
                continue;
            }
            if let Some(p) = by_id.get(id) {
                blobs.push((*id, self.read_for_upload(p, id)?));
            }
        }
        if blobs.is_empty() {
            return Ok(0);
        }
        rpc::upload_blobs(&self.client, blobs).await
    }

    /// The file entry at `path` in the tree `root` (`None` when absent or a directory),
    /// fetching only the trees on the way (verified, cached).
    pub(crate) async fn entry_at(
        &self,
        root: &ObjectId,
        path: &RepoPath,
    ) -> Result<Option<EntryInfo>> {
        let comps: Vec<&[u8]> = path.components().collect();
        let mut tree_id = *root;
        for (i, name) in comps.iter().enumerate() {
            let refused = ensure_trees(&self.fetcher, &self.cache, &[tree_id]).await?;
            if refused.contains(&tree_id) {
                return Err(ScionError::permission_denied(
                    "a tree on the way to a path is not readable",
                ));
            }
            let tree = self.cache.require(&tree_id)?;
            let Some(e) = tree.get(name) else {
                return Ok(None);
            };
            if i + 1 == comps.len() {
                return Ok((e.mode != Mode::Tree).then_some(EntryInfo {
                    mode: e.mode,
                    id: e.id,
                    size: e.size,
                }));
            }
            if e.mode != Mode::Tree {
                return Ok(None);
            }
            tree_id = e.id;
        }
        Ok(None)
    }

    /// One object through GetObjects (verified).
    pub(crate) async fn fetch_item(&self, id: &ObjectId) -> Result<Item> {
        let items = self.fetcher.get_objects(std::slice::from_ref(id)).await?;
        items
            .into_iter()
            .find(|(i, _)| i == id)
            .map(|(_, item)| item)
            .ok_or_else(|| ScionError::not_found(format!("object {id} not returned")))
    }

    /// The Meta object `id` (SPEC 4.3.3; readable by every repository reader unless redacted).
    pub(crate) async fn fetch_meta(&self, id: &ObjectId) -> Result<Meta> {
        match self.fetch_item(id).await? {
            Item::Ok(raw) => Meta::decode(&raw),
            Item::Redacted => Err(ScionError::redacted(id)),
            Item::Denied => Err(ScionError::permission_denied(format!(
                "meta {id} is not readable"
            ))),
            Item::NotFound => Err(ScionError::not_found(format!("meta {id} not found"))),
        }
    }

    /// Move the materialization from the base to `to` within scope: apply the pruned diff
    /// (SPEC 11.3), files already holding their target content are skipped, so a repeated
    /// move after a crash converges. Sets `state.base` (not saved; `idx` not saved).
    pub(crate) async fn move_base(&mut self, to: ObjectId, idx: &mut StatCache) -> Result<()> {
        if to == self.state.base {
            return Ok(());
        }
        let from_root = self.root_tree(&self.state.base).await?;
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
        for c in d.changes.iter().filter(|c| c.new.is_some()) {
            check_entry_path(c.path.as_bytes())?;
        }
        let outcome = self
            .apply_changes(idx, &d.changes, None, Existing::SkipMatching)
            .await?;
        self.state.inaccessible =
            updated_inaccessible(&self.state.inaccessible, &d, &outcome.denied);
        self.state.base = to;
        Ok(())
    }

    /// Restore every dirty in-scope file to its base version and remove untracked files
    /// (SPEC 11.7 `--abort` restores exactly; also the first step of resuming an interrupted
    /// rebase). `idx` is updated, not saved.
    pub(crate) async fn reset_hard(&mut self, idx: &mut StatCache) -> Result<usize> {
        let report = self.status_inner(idx).await?;
        if report.is_clean() {
            return Ok(0);
        }
        let base = self.base_listing().await?.files;
        let changes: Vec<Change> = dirty_paths(&report)
            .into_iter()
            .map(|p| {
                let e = base.get(&p).copied();
                Change {
                    old: e,
                    new: e,
                    path: p,
                }
            })
            .collect();
        let n = changes.len();
        self.apply_changes(idx, &changes, None, Existing::SkipMatching)
            .await?;
        tracing::info!(files = n, "restored dirty files to the base");
        Ok(n)
    }
}
