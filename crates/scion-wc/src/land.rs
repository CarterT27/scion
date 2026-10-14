//! SPEC 11.7 land from a working copy (P28).
//!
//! `land [--ref r]` calls server `Land(r, base)`. On success the base becomes the landed
//! result: the `result` recorded for the base itself in the response's `landed` list, else
//! the returned `target` (a base already contained in the ref lands nothing and moves to the
//! tip). The tracking ref becomes `r`. The materialization then moves like `goto` (journaled
//! as `PendingOperation::Goto`, so an interrupted move is completed by recovery); unrelated
//! dirty files are preserved. When the move would touch a dirty file the working copy is left
//! at its old base (still a valid changeset whose change is now landed) and the report says
//! `moved = false`; landing again later is idempotent.

use crate::checkout::Existing;
use crate::commit::rpc;
use crate::fetch::diff_scope;
use crate::fs_safe::check_entry_path;
use crate::goto::{dirty_paths, touched_by, updated_inaccessible};
use crate::state::PendingOperation;
use crate::wc::WorkingCopy;
use scion_core::{ObjectId, Result, ScionError};
use scion_object::{RefName, RepoPath};
use std::collections::BTreeSet;

/// Result of a land (`--json` schema `scion.cli.land.v1`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct LandReport {
    /// Ref name.
    pub ref_name: String,
    /// New ref target (id text).
    pub target: String,
    /// New ref version.
    pub version: u64,
    /// Landed results (id text).
    pub landed: Vec<String>,
    /// The base after the land (id text): the landed result of the old base.
    pub head: String,
    /// Whether the materialization moved to `head` (false when dirty files were in the way;
    /// the base then stays where it was).
    pub moved: bool,
}

impl WorkingCopy {
    /// Land the base onto `r`; on success the base becomes the landed result (SPEC 11.7).
    pub async fn land(&mut self, r: &RefName) -> Result<LandReport> {
        let _lock = self.begin_mutation().await?;
        if self.state.pending.is_some() {
            return Err(ScionError::invalid(
                WorkingCopy::SUBCODE_REBASE_IN_PROGRESS,
                "land: a rebase is in progress; resolve and commit, or rebase --abort",
            ));
        }
        let base = self.state.base;
        let landed = rpc::land(&self.client, r, &base).await?;
        let result = landed
            .landed
            .iter()
            .find(|(source, _)| *source == base)
            .map(|(_, result)| *result)
            .unwrap_or(landed.target);
        self.state.tracking_ref = Some(r.clone());
        self.save_state()?;
        let moved = self.move_after_land(result).await?;
        tracing::info!(target = %landed.target, version = landed.version, moved, "landed");
        Ok(LandReport {
            ref_name: r.as_str().to_owned(),
            target: landed.target.to_text(),
            version: landed.version,
            landed: landed.landed.iter().map(|(_, r)| r.to_text()).collect(),
            head: if moved { result } else { base }.to_text(),
            moved,
        })
    }

    /// Move the materialization from the base to `to` like goto, refusing (returning false)
    /// when a dirty file would be touched.
    async fn move_after_land(&mut self, to: ObjectId) -> Result<bool> {
        let from = self.state.base;
        if to == from {
            return Ok(true);
        }
        let mut idx = self.load_index()?;
        let status = self.status_inner(&mut idx).await?;
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
        for c in d.changes.iter().filter(|c| c.new.is_some()) {
            check_entry_path(c.path.as_bytes())?;
        }
        let dirty = dirty_paths(&status);
        let deleted: BTreeSet<&RepoPath> = status.deleted.iter().collect();
        let touched = d
            .changes
            .iter()
            .filter(|c| !(c.new.is_none() && deleted.contains(&c.path)))
            .any(|c| !touched_by(&c.path, &dirty).is_empty());
        if touched {
            tracing::warn!(
                "landed, but dirty files are in the way of the landed result; the working copy \
                 stays at its old base"
            );
            self.save_index(&mut idx)?;
            return Ok(false);
        }
        self.state.pending = Some(PendingOperation::Goto { from, to });
        self.save_state()?;
        let outcome = self
            .apply_changes(&mut idx, &d.changes, None, Existing::Overwrite)
            .await?;
        self.state.inaccessible =
            updated_inaccessible(&self.state.inaccessible, &d, &outcome.denied);
        self.state.base = to;
        self.state.pending = None;
        self.save_state()?;
        self.save_index(&mut idx)?;
        Ok(true)
    }
}
