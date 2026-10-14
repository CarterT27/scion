//! Crash recovery of journaled operations (SPEC 11.1, 16.2; P18).

use crate::wc::WorkingCopy;
use scion_core::Result;

impl WorkingCopy {
    /// Complete or roll back a pending Checkout/Goto/SparseChange; dispatch Commit/Rebase to
    /// `commit::recover_commit` / `rebase::recover_rebase`. Returns a description of what was
    /// recovered.
    pub async fn recover(&mut self) -> Result<Option<String>> {
        todo!("P18")
    }
}
