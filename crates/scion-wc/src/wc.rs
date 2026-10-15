//! The [`WorkingCopy`] handle (SPEC 11; P18).
//!
//! Every mutating command takes the `.scion/lock` advisory lock, completes or rolls back any
//! pending operation ([`WorkingCopy::recover`]), replaces clean copies of newly redacted blobs
//! ([`WorkingCopy::refresh_redactions`]) and journals its own `PendingOperation` in
//! `.scion/state` before it touches files (SPEC 11.3, 13.7). The write layer (P28) uses the
//! same entry point, `begin_mutation`, which returns the held lock.

use crate::fetch::{Listing, list_scope};
use crate::fs_safe::SafeRoot;
use crate::ignore::IgnoreMatcher;
use crate::index::StatCache;
use crate::lock::{self, WcLock};
use crate::remote::{Fetcher, root_tree_of};
use crate::state::{self, WcState};
use crate::status::{self, StatusReport};
use crate::tree_cache::TreeCache;
use scion_core::{FaultRegistry, ObjectId, Result, ScionError};
use scion_object::RepoPath;
use scion_proto::ScionClient;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Name of the working-copy metadata directory (SPEC 11.1).
pub(crate) const SCION_DIR: &str = ".scion";

/// Working-copy options.
#[derive(Clone)]
pub struct WcOptions {
    /// Client fault points (SPEC 19.0).
    pub faults: Arc<FaultRegistry>,
    /// Treat the filesystem as case-insensitive (PLAN 6.17).
    pub force_case_insensitive: bool,
    /// Objects per GetObjects batch.
    pub fetch_batch: usize,
}

impl Default for WcOptions {
    fn default() -> Self {
        WcOptions {
            faults: FaultRegistry::new(),
            force_case_insensitive: false,
            fetch_batch: 256,
        }
    }
}

/// An opened working copy (SPEC 4.3.10, 11).
pub struct WorkingCopy {
    pub(crate) root: PathBuf,
    pub(crate) client: ScionClient,
    pub(crate) opts: WcOptions,
    pub(crate) state: WcState,
    pub(crate) safe: SafeRoot,
    pub(crate) cache: TreeCache,
    pub(crate) fetcher: Fetcher,
    pub(crate) collisions: Vec<(RepoPath, RepoPath)>,
}

/// Normalize a sparse profile (SPEC 11.2): an ordered set; the root covers everything.
pub(crate) fn normalize_scope(v: impl IntoIterator<Item = RepoPath>) -> Vec<RepoPath> {
    let mut out: Vec<RepoPath> = Vec::new();
    for p in v {
        if p.is_root() {
            return vec![RepoPath::root()];
        }
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// A dirty-refusal error (SPEC 11.2, 11.3).
pub(crate) fn dirty_error(what: &str, n: usize) -> ScionError {
    ScionError::invalid(
        WorkingCopy::SUBCODE_DIRTY,
        format!("{what}: {n} dirty file(s) would be affected; use force to override"),
    )
}

impl WorkingCopy {
    /// INVALID_ARGUMENT subcode of commands refused because of dirty files (SPEC 11.2, 11.3;
    /// CLI exit code 4, PLAN 6.15).
    pub const SUBCODE_DIRTY: &'static str = "dirty_working_copy";

    /// Open an existing working copy at `root` (SPEC 11.1). A pending operation is not
    /// recovered here; the next command (or [`WorkingCopy::recover`]) does that.
    pub fn open(root: &Path, client: ScionClient, opts: WcOptions) -> Result<Self> {
        let st = state::load(&root.join(SCION_DIR))?;
        Self::assemble(root, client, opts, st)
    }

    pub(crate) fn assemble(
        root: &Path,
        client: ScionClient,
        opts: WcOptions,
        state: WcState,
    ) -> Result<Self> {
        let safe = SafeRoot::open(root)?;
        let cache = TreeCache::open(&root.join(SCION_DIR))?;
        let fetcher = Fetcher::new(client.clone(), opts.fetch_batch);
        Ok(WorkingCopy {
            root: root.to_path_buf(),
            client,
            opts,
            state,
            safe,
            cache,
            fetcher,
            collisions: Vec::new(),
        })
    }

    /// The current state.
    pub fn state(&self) -> &WcState {
        &self.state
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Case collisions found by the last checkout or sparse change: `(written, skipped)`
    /// pairs; the second path of each pair was not written (SPEC 4.4, PLAN 6.17).
    pub fn collisions(&self) -> &[(RepoPath, RepoPath)] {
        &self.collisions
    }

    /// Every object id this handle requested through GetObjects (SPEC 19.10 object set).
    pub fn fetched_objects(&self) -> BTreeSet<ObjectId> {
        self.fetcher.requested()
    }

    /// `<root>/.scion`.
    pub(crate) fn scion_dir(&self) -> PathBuf {
        self.root.join(SCION_DIR)
    }

    /// Persist `.scion/state`.
    pub(crate) fn save_state(&self) -> Result<()> {
        state::save(&self.scion_dir(), &self.state)
    }

    /// Load `.scion/index`.
    pub(crate) fn load_index(&self) -> Result<StatCache> {
        StatCache::load(&self.scion_dir())
    }

    /// Persist `.scion/index`.
    pub(crate) fn save_index(&self, idx: &mut StatCache) -> Result<()> {
        idx.save(&self.scion_dir())
    }

    /// Take the working-copy lock (SPEC 11.1).
    pub(crate) fn lock(&self) -> Result<WcLock> {
        lock::acquire(&self.scion_dir())
    }

    /// Lock, recover any pending operation, then refresh redactions: the common prologue of
    /// every mutating command (SPEC 11.3, 13.7). The returned lock must be held for the rest
    /// of the command.
    pub(crate) async fn begin_mutation(&mut self) -> Result<WcLock> {
        let l = self.lock()?;
        self.recover_inner().await?;
        let dirty = self.refresh_redactions_inner().await?;
        if !dirty.is_empty() {
            tracing::warn!(
                count = dirty.len(),
                "dirty files hold content that is now redacted upstream; left untouched"
            );
        }
        Ok(l)
    }

    /// Root tree of changeset `cs`: from the local cache, else via GetChangesets.
    pub(crate) async fn root_tree(&self, cs: &ObjectId) -> Result<ObjectId> {
        if let Some(t) = self.cache.root_of(cs) {
            return Ok(t);
        }
        let t = root_tree_of(&self.client, cs).await?;
        self.cache.set_root(cs, &t)?;
        Ok(t)
    }

    /// The in-scope listing of the base tree (zero RPCs when cached).
    pub(crate) async fn base_listing(&self) -> Result<Listing> {
        let root = self.root_tree(&self.state.base).await?;
        list_scope(
            &self.fetcher,
            &self.cache,
            &root,
            &self.state.sparse,
            &self.state.inaccessible,
        )
        .await
    }

    /// Status of the scope against the base tree, with `idx` refreshed (not saved).
    pub(crate) async fn status_inner(&mut self, idx: &mut StatCache) -> Result<StatusReport> {
        self.status_with_index_update(idx)
            .await
            .map(|(report, _)| report)
    }

    async fn status_with_index_update(
        &mut self,
        idx: &mut StatCache,
    ) -> Result<(StatusReport, bool)> {
        let root = self.root_tree(&self.state.base).await?;
        let listing = list_scope(
            &self.fetcher,
            &self.cache,
            &root,
            &self.state.sparse,
            &self.state.inaccessible,
        )
        .await?;
        let mut inaccessible = self.state.inaccessible.clone();
        inaccessible.extend(listing.denied.iter().cloned());
        let ign = IgnoreMatcher::load(&self.root)?;
        status::status_files(
            &self.safe,
            idx,
            &ign,
            &self.state.sparse,
            &inaccessible,
            listing.files,
        )
    }

    /// SPEC 11.5 status of the sparse scope (zero RPCs when trees are cached). A pending
    /// operation is completed or rolled back first (SPEC 11.3).
    pub async fn status(&mut self) -> Result<StatusReport> {
        let _l = self.lock()?;
        if self.state.pending.is_some() {
            self.recover_inner().await?;
        }
        let mut idx = self.load_index()?;
        let (report, index_changed) = self.status_with_index_update(&mut idx).await?;
        if index_changed {
            self.save_index(&mut idx)?;
        }
        Ok(report)
    }
}
