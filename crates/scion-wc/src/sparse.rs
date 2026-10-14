//! SPEC 11.2 sparse profile changes (P18).
//!
//! `sparse_add` materializes exactly the files that enter the scope; `sparse_remove` removes
//! exactly the files that leave it and refuses (unless `force`) when any of them, or any
//! untracked file in the leaving region, is dirty. Both journal
//! `PendingOperation::SparseChange` before touching files (fault point `wc.sparse.mid_write`)
//! and keep the old profile in the state until they finish, so recovery can roll forward.

use crate::checkout::Existing;
use crate::fetch::{Change, Listing, list_scope};
use crate::goto::dirty_paths;
use crate::points;
use crate::state::PendingOperation;
use crate::status::Prefixes;
use crate::wc::{WorkingCopy, dirty_error, normalize_scope};
use scion_core::{Result, ScionError, subcode};
use scion_object::RepoPath;
use std::collections::BTreeSet;

impl WorkingCopy {
    async fn listing_for(&self, scope: &[RepoPath]) -> Result<Listing> {
        let root = self.root_tree(&self.state.base).await?;
        list_scope(
            &self.fetcher,
            &self.cache,
            &root,
            scope,
            &self.state.inaccessible,
        )
        .await
    }

    /// Add prefixes to the sparse profile and materialize exactly the affected files
    /// (SPEC 11.2).
    pub async fn sparse_add(&mut self, p: &[RepoPath]) -> Result<()> {
        let _lock = self.begin_mutation().await?;
        let old = self.state.sparse.clone();
        let new = normalize_scope(old.iter().cloned().chain(p.iter().cloned()));
        if new == old {
            return Ok(());
        }
        let old_l = self.listing_for(&old).await?;
        let new_l = self.listing_for(&new).await?;
        // Validate against the new scope before any write.
        let entering: BTreeSet<RepoPath> = new_l
            .files
            .keys()
            .filter(|f| !old_l.files.contains_key(*f))
            .cloned()
            .collect();
        let saved_sparse = std::mem::replace(&mut self.state.sparse, new.clone());
        let plan = if entering.is_empty() {
            // Only directories on the way (or nothing) are affected.
            let mut plan = self.plan(&new_l, &BTreeSet::new())?;
            plan.files.clear();
            plan
        } else {
            self.plan(&new_l, &entering)?
        };
        self.state.sparse = saved_sparse;
        self.state.pending = Some(PendingOperation::SparseChange {
            old: old.clone(),
            new: new.clone(),
        });
        self.save_state()?;
        let mut idx = self.load_index()?;
        self.state.sparse = new.clone();
        let result = self.write_sparse_plan(&mut idx, plan, &new_l).await;
        if let Err(e) = result {
            // The journal still records the old profile; keep the in-memory state consistent.
            self.state.sparse = old;
            return Err(e);
        }
        self.state.pending = None;
        self.save_state()?;
        self.save_index(&mut idx)
    }

    async fn write_sparse_plan(
        &mut self,
        idx: &mut crate::index::StatCache,
        plan: crate::checkout::Plan,
        new_l: &Listing,
    ) -> Result<()> {
        self.collisions = plan.collisions.clone();
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
            .apply_changes(
                idx,
                &changes,
                Some(points::SPARSE_MID_WRITE),
                Existing::KeepDiffering,
            )
            .await?;
        self.add_inaccessible(new_l.denied.iter().cloned());
        self.add_inaccessible(outcome.denied);
        Ok(())
    }

    /// Remove prefixes; fails on dirty files unless `force` (SPEC 11.2). Each prefix must be
    /// an entry of the profile.
    pub async fn sparse_remove(&mut self, p: &[RepoPath], force: bool) -> Result<()> {
        let _lock = self.begin_mutation().await?;
        let old = self.state.sparse.clone();
        for q in p {
            if !old.contains(q) {
                return Err(ScionError::invalid(
                    subcode::INVALID_PATH,
                    "a prefix to remove is not in the sparse profile",
                ));
            }
        }
        let new: Vec<RepoPath> = old.iter().filter(|q| !p.contains(q)).cloned().collect();
        if new == old {
            return Ok(());
        }
        let mut idx = self.load_index()?;
        let status = self.status_inner(&mut idx).await?;
        let new_scope = Prefixes::new(&new, &[]);
        let dirty_leaving: Vec<RepoPath> = dirty_paths(&status)
            .into_iter()
            .filter(|d| !status.deleted.contains(d) && !new_scope.in_scope(d.as_bytes()))
            .collect();
        if !dirty_leaving.is_empty() && !force {
            return Err(dirty_error("sparse remove", dirty_leaving.len()));
        }
        let changes = self.leaving_changes(&old, &new).await?;
        self.state.pending = Some(PendingOperation::SparseChange {
            old: old.clone(),
            new: new.clone(),
        });
        self.save_state()?;
        self.apply_changes(
            &mut idx,
            &changes,
            Some(points::SPARSE_MID_WRITE),
            Existing::Overwrite,
        )
        .await?;
        self.finish_sparse(&mut idx, new)
    }

    /// Removals for files leaving the scope.
    async fn leaving_changes(&self, old: &[RepoPath], new: &[RepoPath]) -> Result<Vec<Change>> {
        let old_l = self.listing_for(old).await?;
        let new_l = self.listing_for(new).await?;
        Ok(old_l
            .files
            .iter()
            .filter(|(f, _)| !new_l.files.contains_key(*f))
            .map(|(f, e)| Change {
                path: f.clone(),
                old: Some(*e),
                new: None,
            })
            .collect())
    }

    fn finish_sparse(
        &mut self,
        idx: &mut crate::index::StatCache,
        new: Vec<RepoPath>,
    ) -> Result<()> {
        let scope = Prefixes::new(&new, &[]);
        idx.entries.retain(|p, _| scope.in_scope(p.as_bytes()));
        self.state
            .inaccessible
            .retain(|p| scope.in_scope(p.as_bytes()) || scope.leads_to_scope(p.as_bytes()));
        self.state.sparse = new;
        self.state.pending = None;
        self.save_state()?;
        self.save_index(idx)
    }

    /// Roll an interrupted sparse change forward (SPEC 11.3): materialize what enters the
    /// scope and remove what leaves it.
    pub(crate) async fn complete_sparse(
        &mut self,
        old: Vec<RepoPath>,
        new: Vec<RepoPath>,
    ) -> Result<()> {
        let mut idx = self.load_index()?;
        let removals = self.leaving_changes(&old, &new).await?;
        self.apply_changes(&mut idx, &removals, None, Existing::Overwrite)
            .await?;
        let old_l = self.listing_for(&old).await?;
        let new_l = self.listing_for(&new).await?;
        let entering: BTreeSet<RepoPath> = new_l
            .files
            .keys()
            .filter(|f| !old_l.files.contains_key(*f))
            .cloned()
            .collect();
        self.state.sparse = new.clone();
        if !entering.is_empty() {
            let plan = self.plan(&new_l, &entering)?;
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
                .apply_changes(&mut idx, &changes, None, Existing::KeepDiffering)
                .await?;
            self.add_inaccessible(outcome.denied);
        }
        self.add_inaccessible(new_l.denied.iter().cloned());
        self.finish_sparse(&mut idx, new)
    }
}
