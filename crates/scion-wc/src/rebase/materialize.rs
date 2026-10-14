//! Conflict materialization of a paused rebase (SPEC 11.7; P28).
//!
//! Clean entries are written from their blobs. A clean content merge produced by the server
//! is stored as a blob without `object_paths` rows, which GetObjects never serves (SPEC 12.5),
//! so such an entry is recomputed locally with the same engine (`scion_merge::merge3`, the
//! normative line-diff3-v1) from the base, ours and theirs versions, and checked against the
//! reported id.

use super::{CONFLICTS_DIR, RebaseReport, RebaseState, lossy};
use crate::commit::rpc::{self, CsInfo};
use crate::commit::write_rebase_clean;
use crate::fs_safe::check_entry_path;
use crate::index::{IndexEntry, StatCache};
use crate::placeholder::placeholder_bytes;
use crate::points;
use crate::remote::Item;
use crate::status::Prefixes;
use crate::wc::WorkingCopy;
use scion_core::{
    Conflict, ConflictKind, ConflictReport, EntryInfo, ErrorCode, Mode, ObjectId, Result,
    ScionError,
};
use scion_merge::{Merge3, merge_mode, merge3, render_markers};
use scion_object::RepoPath;
use std::collections::BTreeMap;

impl WorkingCopy {
    /// Pause the rebase at member `m` (rebased onto `prev`) with the server's conflict
    /// report: move to `prev`, write the clean entries and the conflicts, then record the
    /// conflicting paths in the journal.
    pub(super) async fn materialize_conflicts(
        &mut self,
        mut st: RebaseState,
        m: ObjectId,
        prev: ObjectId,
        report: ConflictReport,
    ) -> Result<RebaseReport> {
        let scope = Prefixes::new(&self.state.sparse, &self.state.inaccessible);
        let materializable = |p: &[u8]| scope.in_scope(p) && !scope.inaccessible(p);
        if report.conflicts.is_empty() {
            self.abort_to_original().await?;
            return Err(ScionError::permission_denied(
                "the rebase conflicts at paths you cannot read; the rebase was aborted",
            ));
        }
        if report.conflicts.iter().any(|c| !materializable(&c.path)) {
            self.abort_to_original().await?;
            return Err(ScionError::invalid(
                WorkingCopy::SUBCODE_OUT_OF_SCOPE,
                "the rebase conflicts outside the sparse scope; widen the scope and retry \
                 (the rebase was aborted)",
            ));
        }
        let member = rpc::fetch_changeset(&self.client, &m).await?;
        let mut idx = self.load_index()?;
        // Onto's merged state: the previous result (or onto itself).
        self.reset_hard(&mut idx).await?;
        self.move_base(prev, &mut idx).await?;
        self.save_state()?;
        self.save_index(&mut idx)?;

        let mut inside: Vec<(RepoPath, Option<EntryInfo>)> = Vec::new();
        let mut outside: Vec<(RepoPath, Option<EntryInfo>)> = Vec::new();
        for c in &report.clean_entries {
            let p = RepoPath::parse(&c.path)?;
            if materializable(&c.path) {
                check_entry_path(&c.path)?;
                inside.push((p, c.entry));
            } else {
                outside.push((p, c.entry));
            }
        }
        write_rebase_clean(&self.scion_dir(), &outside)?;
        let prev_root = self.root_tree(&prev).await?;
        self.write_clean(&mut idx, &inside, &member, &prev_root)
            .await?;
        self.save_index(&mut idx)?;

        let conflicts_dir = self.scion_dir().join(CONFLICTS_DIR);
        if let Err(e) = std::fs::remove_dir_all(&conflicts_dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(crate::fs_safe::io_error("clear .scion/conflicts", e));
        }
        let mut paths = Vec::with_capacity(report.conflicts.len());
        for c in &report.conflicts {
            self.write_conflict(&mut idx, c).await?;
            paths.push(c.path.clone());
        }
        self.save_index(&mut idx)?;
        self.opts
            .faults
            .check(points::REBASE_AFTER_CONFLICT_WRITE)
            .await?;
        paths.sort();
        paths.dedup();
        st.current = Some(m);
        st.conflicts = paths.clone();
        st.resolved = Vec::new();
        self.state.pending = Some(st.clone().into_pending());
        self.save_state()?;
        tracing::info!(
            member = %m,
            conflicts = paths.len(),
            "rebase paused on conflicts"
        );
        Ok(st.report(paths.iter().map(|p| lossy(p)).collect(), None))
    }

    /// Write one file (or the placeholder of a redacted blob, SPEC 13.7) and index it.
    fn write_entry(
        &self,
        idx: &mut StatCache,
        p: &RepoPath,
        e: &EntryInfo,
        content: Option<&[u8]>,
    ) -> Result<()> {
        match content {
            Some(data) => self.safe.write_file_atomic(p, data, e.mode)?,
            None => {
                let mode = if e.mode == Mode::Executable {
                    Mode::Executable
                } else {
                    Mode::File
                };
                self.safe
                    .write_file_atomic(p, &placeholder_bytes(&e.id), mode)?;
            }
        }
        match self.safe.stat(p)? {
            Some(stat) => {
                idx.entries.insert(
                    p.clone(),
                    IndexEntry {
                        stat,
                        id: e.id,
                        mode: e.mode,
                        redacted: content.is_none(),
                    },
                );
            }
            None => {
                idx.entries.remove(p);
            }
        }
        Ok(())
    }

    /// Remove a file at `p` (never a directory).
    fn remove_file_at(&self, idx: &mut StatCache, p: &RepoPath) -> Result<()> {
        if let Some(st) = self.safe.stat(p)?
            && !st.is_dir()
        {
            self.safe.remove(p)?;
        }
        idx.entries.remove(p);
        Ok(())
    }

    /// Write the in-scope clean entries of a conflict report.
    async fn write_clean(
        &mut self,
        idx: &mut StatCache,
        entries: &[(RepoPath, Option<EntryInfo>)],
        member: &CsInfo,
        prev_root: &ObjectId,
    ) -> Result<()> {
        let mut by_blob: BTreeMap<ObjectId, Vec<(RepoPath, EntryInfo)>> = BTreeMap::new();
        for (p, e) in entries {
            match e {
                None => self.remove_file_at(idx, p)?,
                Some(e) => {
                    if let Some(ie) = self.disk_matches(p, e)? {
                        idx.entries.insert(p.clone(), ie);
                    } else {
                        by_blob.entry(e.id).or_default().push((p.clone(), *e));
                    }
                }
            }
        }
        if by_blob.is_empty() {
            return Ok(());
        }
        let ids: Vec<ObjectId> = by_blob.keys().copied().collect();
        for (id, item) in self.fetcher.get_objects(&ids).await? {
            for (p, e) in by_blob.remove(&id).unwrap_or_default() {
                match &item {
                    Item::Ok(data) => self.write_entry(idx, &p, &e, Some(data))?,
                    Item::Redacted => self.write_entry(idx, &p, &e, None)?,
                    Item::Denied | Item::NotFound => {
                        let merged = self.local_merge(&p, &e, member, prev_root).await?;
                        self.write_entry(idx, &p, &e, Some(&merged))?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Recompute a clean server-side content merge locally (line-diff3-v1) and check it
    /// against the reported blob id.
    async fn local_merge(
        &self,
        p: &RepoPath,
        e: &EntryInfo,
        member: &CsInfo,
        prev_root: &ObjectId,
    ) -> Result<Vec<u8>> {
        let unavailable = || {
            ScionError::new(
                ErrorCode::Unavailable,
                format!(
                    "the merged content of a clean rebase entry ({}) is unavailable",
                    e.id
                ),
            )
        };
        let Some(parent) = member.parents.first() else {
            return Err(unavailable());
        };
        let parent_root = self.root_tree(parent).await?;
        let b = self.entry_at(&parent_root, p).await?;
        let o = self.entry_at(prev_root, p).await?;
        let t = self.entry_at(&member.root_tree, p).await?;
        let (Some(b), Some(o), Some(t)) = (b, o, t) else {
            return Err(unavailable());
        };
        let mut contents = Vec::with_capacity(3);
        for x in [b, o, t] {
            match self.fetch_item(&x.id).await? {
                Item::Ok(data) => contents.push(data),
                _ => return Err(unavailable()),
            }
        }
        match merge3(&contents[0], &contents[1], &contents[2]) {
            Merge3::Clean(merged) if ObjectId::hash_blob(&merged) == e.id => Ok(merged),
            Merge3::Clean(_) => Err(ScionError::integrity(&e.id)),
            Merge3::Conflict(_) => Err(unavailable()),
        }
    }

    /// The content of a conflict side: `None` when it cannot be read.
    async fn side_content(&self, e: &EntryInfo) -> Result<Option<Vec<u8>>> {
        Ok(match self.fetch_item(&e.id).await? {
            Item::Ok(data) => Some(data.to_vec()),
            Item::Redacted => Some(placeholder_bytes(&e.id)),
            Item::Denied | Item::NotFound => None,
        })
    }

    /// Materialize one conflict (SPEC 11.7).
    async fn write_conflict(&mut self, idx: &mut StatCache, c: &Conflict) -> Result<()> {
        let p = RepoPath::parse(&c.path)?;
        check_entry_path(&c.path)?;
        if c.kind == ConflictKind::Content
            && let (Some(b), Some(o), Some(t)) = (c.base, c.ours, c.theirs)
        {
            let (cb, co, ct) = (
                self.side_content(&b).await?,
                self.side_content(&o).await?,
                self.side_content(&t).await?,
            );
            if let (Some(cb), Some(co), Some(ct)) = (cb, co, ct) {
                let text = render_markers(&cb, &co, &ct);
                let mode = match merge_mode(b.mode, o.mode, t.mode) {
                    Mode::Executable => Mode::Executable,
                    _ => Mode::File,
                };
                self.safe.write_file_atomic(&p, &text, mode)?;
                // Not a committed version: status hashes it and reports it modified.
                idx.entries.remove(&p);
                return Ok(());
            }
        }
        // Other kinds (and unreadable text sides): keep ours, save every side.
        match &c.ours {
            Some(o) => {
                if let Some(ie) = self.disk_matches(&p, o)? {
                    idx.entries.insert(p.clone(), ie);
                } else {
                    let content = match self.fetch_item(&o.id).await? {
                        Item::Ok(data) => Some(data.to_vec()),
                        Item::Redacted => None,
                        Item::Denied | Item::NotFound => {
                            return Err(ScionError::permission_denied(
                                "the ours version of a conflict is not readable",
                            ));
                        }
                    };
                    self.write_entry(idx, &p, o, content.as_deref())?;
                }
            }
            None => self.remove_file_at(idx, &p)?,
        }
        for (side, entry) in [("base", c.base), ("ours", c.ours), ("theirs", c.theirs)] {
            let Some(e) = entry else { continue };
            let Some(data) = self.side_content(&e).await? else {
                tracing::warn!(
                    side,
                    "a conflict side is not readable; no side file written"
                );
                continue;
            };
            let path = self.conflict_side_path(&p, side);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|err| crate::fs_safe::io_error("create .scion/conflicts", err))?;
            }
            std::fs::write(&path, &data)
                .map_err(|err| crate::fs_safe::io_error("write a conflict side file", err))?;
        }
        Ok(())
    }
}
