//! Crash recovery of journaled operations (SPEC 11.1, 11.3, 16.2; P18).
//!
//! Checkout, Goto and SparseChange are rolled forward: their journal records both endpoints,
//! every file write is atomic and idempotent, and files that already hold their target content
//! are skipped, so re-applying the operation reaches exactly the state the uninterrupted
//! command would have produced. Commit and Rebase are dispatched to the write layer (P28).

use crate::state::PendingOperation;
use crate::wc::WorkingCopy;
use scion_core::Result;

impl WorkingCopy {
    /// Complete or roll back a pending Checkout/Goto/SparseChange; dispatch Commit/Rebase to
    /// `commit::recover_commit` / `rebase::recover_rebase`. Returns a description of what was
    /// recovered (SPEC 11.3: a command that finds a pending operation completes or rolls it
    /// back before doing anything else).
    pub async fn recover(&mut self) -> Result<Option<String>> {
        let _l = self.lock()?;
        self.recover_inner().await
    }

    /// [`WorkingCopy::recover`] with the lock already held.
    pub(crate) async fn recover_inner(&mut self) -> Result<Option<String>> {
        let Some(pending) = self.state.pending.clone() else {
            return Ok(None);
        };
        match pending {
            PendingOperation::Checkout { target } => {
                self.state.base = target;
                self.rematerialize(None).await?;
                self.state.pending = None;
                self.save_state()?;
                tracing::info!("completed an interrupted checkout");
                Ok(Some(format!("completed interrupted checkout of {target}")))
            }
            PendingOperation::Goto { from, to } => {
                self.complete_goto(from, to).await?;
                tracing::info!("completed an interrupted goto");
                Ok(Some(format!(
                    "completed interrupted goto from {from} to {to}"
                )))
            }
            PendingOperation::SparseChange { old, new } => {
                self.complete_sparse(old, new).await?;
                tracing::info!("completed an interrupted sparse change");
                Ok(Some(
                    "completed interrupted sparse profile change".to_owned(),
                ))
            }
            PendingOperation::Commit { .. } => crate::commit::recover_commit(self).await,
            PendingOperation::Rebase { .. } => crate::rebase::recover_rebase(self).await,
        }
    }
}
