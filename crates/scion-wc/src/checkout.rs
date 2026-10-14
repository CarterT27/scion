//! SPEC 11.3 checkout (P18), plus the file-application machinery shared by goto, sparse
//! changes and recovery.
//!
//! Checkout order (SPEC 10.1, 11.3, 17):
//! 1. resolve the revision and its root tree;
//! 2. fetch the in-scope trees breadth-first (verified; a malicious tree fails SPEC 4
//!    validation here);
//! 3. validate every entry path (SPEC 4.4, never `.scion`), compute case collisions when the
//!    filesystem is case-insensitive (PLAN 6.17), and refuse pre-existing symlinks or files
//!    where directories must go, all BEFORE any write; on refusal the new `.scion` is removed;
//! 4. journal `PendingOperation::Checkout`, then stream the blobs in batches and write each
//!    file atomically through [`SafeRoot`](crate::fs_safe::SafeRoot) (exec bits, symlinks;
//!    redacted blobs as placeholders marked `redacted` in the index, SPEC 13.7);
//! 5. save the index and the state with the pending operation cleared.

use crate::fetch::{Change, Listing, list_scope};
use crate::fs_safe::{case_collisions, check_entry_path};
use crate::index::{IndexEntry, StatCache};
use crate::placeholder::placeholder_bytes;
use crate::points;
use crate::remote::{Item, resolve_rev};
use crate::state::{PendingOperation, WcState};
use crate::status::Prefixes;
use crate::wc::{SCION_DIR, WcOptions, WorkingCopy, dirty_error, normalize_scope};
use scion_core::{EntryInfo, ErrorCode, Mode, ObjectId, Result, ScionError, subcode};
use scion_object::RepoPath;
use scion_proto::ScionClient;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Counts of an [`WorkingCopy::apply_changes`] run.
#[derive(Debug, Default)]
pub(crate) struct ApplyOutcome {
    pub written: u64,
    pub removed: u64,
    /// File paths whose blob upstream refused (`denied`): recorded as inaccessible.
    pub denied: Vec<RepoPath>,
}

/// How [`WorkingCopy::apply_changes`] treats files already on disk.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Existing {
    /// Always (re)write targets.
    Overwrite,
    /// Skip targets whose disk content already matches (recovery, checkout resume).
    SkipMatching,
    /// Skip matching targets and never overwrite a differing file (sparse add).
    KeepDiffering,
}

/// A validated materialization plan (SPEC 11.3, 17).
#[derive(Debug, Default)]
pub(crate) struct Plan {
    pub files: Vec<(RepoPath, EntryInfo)>,
    pub dirs_on_the_way: Vec<RepoPath>,
    pub collisions: Vec<(RepoPath, RepoPath)>,
    /// Target files that already exist on disk (regular files or symlinks).
    pub existing: Vec<RepoPath>,
}

fn blocked(msg: &str) -> ScionError {
    ScionError::invalid(subcode::INVALID_PATH, msg)
}

impl WorkingCopy {
    /// Validate every path and case collision BEFORE any write, then materialize `rev` within
    /// `sparse` at `root` (SPEC 10.1, 11.3, 17). An empty `sparse` means the whole tree.
    pub async fn checkout(
        client: ScionClient,
        root: &Path,
        rev: &str,
        sparse: Vec<RepoPath>,
        opts: WcOptions,
    ) -> Result<Self> {
        let sparse = if sparse.is_empty() {
            vec![RepoPath::root()]
        } else {
            normalize_scope(sparse)
        };
        std::fs::create_dir_all(root)
            .map_err(|e| crate::fs_safe::io_error("create working-copy root", e))?;
        let scion = root.join(SCION_DIR);
        if std::fs::symlink_metadata(&scion).is_ok() {
            return Err(ScionError::invalid(
                subcode::INVALID_PATH,
                "the target directory is already a scion working copy",
            ));
        }
        let (target, tracking) = resolve_rev(&client, rev).await?;
        std::fs::create_dir(&scion).map_err(|e| crate::fs_safe::io_error("create .scion", e))?;
        let st = WcState {
            endpoint: None,
            base: target,
            tracking_ref: tracking,
            sparse,
            inaccessible: Vec::new(),
            pending: None,
        };
        let prepared = async {
            let wc = WorkingCopy::assemble(root, client, opts, st)?;
            let lock = wc.lock()?;
            let listing = wc.base_listing().await?;
            let plan = wc.plan(&listing, &BTreeSet::new())?;
            wc.refuse_differing_existing(&plan)?;
            Ok::<_, ScionError>((wc, lock, listing, plan))
        }
        .await;
        let (mut wc, lock, listing, plan) = match prepared {
            Ok(v) => v,
            Err(e) => {
                // Nothing outside `.scion` was written: remove it so the root stays as it was.
                if let Err(rm) = std::fs::remove_dir_all(&scion) {
                    tracing::warn!(error = %rm, "could not remove .scion after a refused checkout");
                }
                return Err(e);
            }
        };
        wc.state.inaccessible = listing.denied.clone();
        wc.state.pending = Some(PendingOperation::Checkout { target });
        wc.save_state()?;
        let mut idx = StatCache::default();
        wc.save_index(&mut idx)?;
        wc.write_plan(&mut idx, plan, Some(points::CHECKOUT_MID_WRITE))
            .await?;
        wc.state.pending = None;
        wc.save_state()?;
        wc.save_index(&mut idx)?;
        drop(lock);
        Ok(wc)
    }

    /// Validate a listing for materialization (SPEC 4.4, 17, PLAN 6.17): every path, case
    /// collisions (when the filesystem is or is forced case-insensitive), and no pre-existing
    /// symlink or file where a directory must go. `only` (when non-empty) restricts the files
    /// to materialize. Nothing is written.
    pub(crate) fn plan(&self, listing: &Listing, only: &BTreeSet<RepoPath>) -> Result<Plan> {
        for p in listing.files.keys().chain(listing.dirs.iter()) {
            check_entry_path(p.as_bytes())?;
        }
        let mut skip = BTreeSet::new();
        let mut collisions = Vec::new();
        if self.opts.force_case_insensitive || self.safe.is_case_insensitive() {
            let paths: Vec<RepoPath> = listing.files.keys().cloned().collect();
            collisions = case_collisions(&paths);
            for (_, later) in &collisions {
                skip.insert(later.clone());
            }
            if !collisions.is_empty() {
                tracing::warn!(
                    count = collisions.len(),
                    "case collisions: colliding entries are reported and not written"
                );
            }
        }
        let files: Vec<(RepoPath, EntryInfo)> = listing
            .files
            .iter()
            .filter(|(p, _)| !skip.contains(*p) && (only.is_empty() || only.contains(*p)))
            .map(|(p, e)| (p.clone(), *e))
            .collect();
        // Directories that will be created or written through must be real directories.
        let mut dirs: BTreeSet<RepoPath> = BTreeSet::new();
        for (p, _) in &files {
            for a in p.ancestors() {
                if !a.is_root() {
                    dirs.insert(a);
                }
            }
        }
        let scope = Prefixes::new(&self.state.sparse, &[]);
        let dirs_on_the_way: Vec<RepoPath> = listing
            .dirs
            .iter()
            .filter(|d| !scope.in_scope(d.as_bytes()))
            .cloned()
            .collect();
        dirs.extend(dirs_on_the_way.iter().cloned());
        // Parents sort before their children, so a directory whose parent is absent is
        // absent too and needs no stat (keeps deep trees linear).
        let mut absent: BTreeSet<RepoPath> = BTreeSet::new();
        let parent_absent = |p: &RepoPath, absent: &BTreeSet<RepoPath>| {
            p.parent().is_some_and(|q| absent.contains(&q))
        };
        for d in &dirs {
            if parent_absent(d, &absent) {
                absent.insert(d.clone());
                continue;
            }
            match self.safe.stat(d)? {
                None => {
                    absent.insert(d.clone());
                }
                Some(st) if !st.is_dir() => {
                    return Err(blocked(
                        "a pre-existing symlink or file is in the way of a directory; refusing to write through it",
                    ));
                }
                Some(_) => {}
            }
        }
        let mut existing = Vec::new();
        for (p, _) in &files {
            if parent_absent(p, &absent) {
                continue;
            }
            match self.safe.stat(p)? {
                Some(st) if st.is_dir() => {
                    return Err(ScionError::invalid(
                        subcode::FILE_DIRECTORY,
                        "a pre-existing directory is in the way of a file",
                    ));
                }
                Some(_) => existing.push(p.clone()),
                None => {}
            }
        }
        Ok(Plan {
            files,
            dirs_on_the_way,
            collisions,
            existing,
        })
    }

    /// Refuse a checkout that would overwrite pre-existing files with different content
    /// (local data is never silently overwritten; files already holding their target content
    /// are adopted). Checked before any write.
    fn refuse_differing_existing(&self, plan: &Plan) -> Result<()> {
        if plan.existing.is_empty() {
            return Ok(());
        }
        let targets: BTreeMap<&RepoPath, &EntryInfo> =
            plan.files.iter().map(|(p, e)| (p, e)).collect();
        let mut differing = 0usize;
        for p in &plan.existing {
            if let Some(e) = targets.get(p)
                && self.disk_matches(p, e)?.is_none()
            {
                differing += 1;
            }
        }
        if differing > 0 {
            return Err(dirty_error("checkout", differing));
        }
        Ok(())
    }

    /// Write a validated plan: directories on the way to scope prefixes (SPEC 11.2), then
    /// every file (skipping files whose disk content already matches).
    pub(crate) async fn write_plan(
        &mut self,
        idx: &mut StatCache,
        plan: Plan,
        point: Option<&str>,
    ) -> Result<ApplyOutcome> {
        self.collisions = plan.collisions;
        for d in &plan.dirs_on_the_way {
            self.safe.create_dir_all(d)?;
        }
        let changes: Vec<Change> = plan
            .files
            .into_iter()
            .map(|(path, e)| Change {
                path,
                old: None,
                new: Some(e),
            })
            .collect();
        let outcome = self
            .apply_changes(idx, &changes, point, Existing::SkipMatching)
            .await?;
        self.add_inaccessible(outcome.denied.iter().cloned());
        Ok(outcome)
    }

    /// Record inaccessible prefixes (sorted, deduplicated).
    pub(crate) fn add_inaccessible(&mut self, extra: impl IntoIterator<Item = RepoPath>) {
        let mut set: BTreeSet<RepoPath> = self.state.inaccessible.drain(..).collect();
        set.extend(extra);
        self.state.inaccessible = set.into_iter().collect();
    }

    /// Whether the file at `path` already holds `e` (or its redaction placeholder). Returns
    /// the index entry to record when it does.
    pub(crate) fn disk_matches(
        &self,
        path: &RepoPath,
        e: &EntryInfo,
    ) -> Result<Option<IndexEntry>> {
        let Some(st) = self.safe.stat(path)? else {
            return Ok(None);
        };
        let Some(disk_mode) = st.entry_mode() else {
            return Ok(None);
        };
        let content = match disk_mode {
            Mode::Symlink => self.safe.read_link(path),
            _ => self.safe.read(path),
        };
        let content = match content {
            Ok(c) => c,
            Err(err) if err.code == ErrorCode::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        if ObjectId::hash_blob(&content) == e.id && disk_mode == e.mode {
            return Ok(Some(IndexEntry {
                stat: st,
                id: e.id,
                mode: e.mode,
                redacted: false,
            }));
        }
        if content == placeholder_bytes(&e.id) {
            return Ok(Some(IndexEntry {
                stat: st,
                id: e.id,
                mode: e.mode,
                redacted: true,
            }));
        }
        Ok(None)
    }

    /// Write one file (or its placeholder) and record it in the index.
    fn write_one(
        &self,
        idx: &mut StatCache,
        path: &RepoPath,
        e: &EntryInfo,
        content: Option<&[u8]>,
    ) -> Result<()> {
        let redacted = content.is_none();
        match content {
            Some(data) => self.safe.write_file_atomic(path, data, e.mode)?,
            None => {
                // SPEC 13.7 placeholder; a redacted symlink is materialized as a regular file.
                let mode = if e.mode == Mode::Executable {
                    Mode::Executable
                } else {
                    Mode::File
                };
                self.safe
                    .write_file_atomic(path, &placeholder_bytes(&e.id), mode)?;
            }
        }
        match self.safe.stat(path)? {
            Some(stat) => {
                idx.entries.insert(
                    path.clone(),
                    IndexEntry {
                        stat,
                        id: e.id,
                        mode: e.mode,
                        redacted,
                    },
                );
            }
            None => {
                idx.entries.remove(path);
            }
        }
        Ok(())
    }

    async fn checkpoint(&self, point: Option<&str>) -> Result<()> {
        match point {
            Some(p) => self.opts.faults.check(p).await,
            None => Ok(()),
        }
    }

    /// Apply file-level changes: removals first, then writes, fetching the needed blobs in
    /// batches (verified, resumable). `point` is checked after every file operation
    /// (SPEC 19.0 client fault points).
    pub(crate) async fn apply_changes(
        &mut self,
        idx: &mut StatCache,
        changes: &[Change],
        point: Option<&str>,
        existing: Existing,
    ) -> Result<ApplyOutcome> {
        let mut out = ApplyOutcome::default();
        for c in changes.iter().filter(|c| c.new.is_none()) {
            self.safe.remove(&c.path)?;
            idx.entries.remove(&c.path);
            out.removed += 1;
            self.checkpoint(point).await?;
        }
        let mut by_blob: BTreeMap<ObjectId, Vec<(RepoPath, EntryInfo, bool)>> = BTreeMap::new();
        for c in changes {
            let Some(e) = c.new else { continue };
            if existing != Existing::Overwrite {
                if let Some(entry) = self.disk_matches(&c.path, &e)? {
                    idx.entries.insert(c.path.clone(), entry);
                    continue;
                }
                if existing == Existing::KeepDiffering && self.safe.stat(&c.path)?.is_some() {
                    tracing::debug!("keeping a pre-existing differing file in place");
                    continue;
                }
            }
            by_blob
                .entry(e.id)
                .or_default()
                .push((c.path.clone(), e, c.old.is_some()));
        }
        let ids: Vec<ObjectId> = by_blob.keys().copied().collect();
        for chunk in ids.chunks(self.opts.fetch_batch.max(1)) {
            let items = self.fetcher.get_objects(chunk).await?;
            for (id, item) in items {
                let targets = by_blob.remove(&id).unwrap_or_default();
                for (path, e, had_old) in targets {
                    match &item {
                        Item::Ok(data) => self.write_one(idx, &path, &e, Some(data))?,
                        Item::Redacted => self.write_one(idx, &path, &e, None)?,
                        Item::Denied => {
                            // Not readable (SPEC 12.6): recorded as inaccessible and, as in a
                            // fresh checkout, not materialized; the replaced old version goes.
                            if had_old {
                                self.safe.remove(&path)?;
                                idx.entries.remove(&path);
                                out.removed += 1;
                                self.checkpoint(point).await?;
                            }
                            out.denied.push(path);
                            continue;
                        }
                        Item::NotFound => {
                            return Err(ScionError::not_found(format!(
                                "blob {id} not found upstream"
                            )));
                        }
                    }
                    out.written += 1;
                    self.checkpoint(point).await?;
                }
            }
        }
        Ok(out)
    }

    /// Re-materialize the base within scope (completion of an interrupted checkout).
    pub(crate) async fn rematerialize(&mut self, point: Option<&str>) -> Result<ApplyOutcome> {
        let root = self.root_tree(&self.state.base).await?;
        let listing = list_scope(
            &self.fetcher,
            &self.cache,
            &root,
            &self.state.sparse,
            &self.state.inaccessible,
        )
        .await?;
        let plan = self.plan(&listing, &BTreeSet::new())?;
        self.add_inaccessible(listing.denied.iter().cloned());
        let mut idx = self.load_index()?;
        let out = self.write_plan(&mut idx, plan, point).await?;
        self.save_index(&mut idx)?;
        Ok(out)
    }
}
