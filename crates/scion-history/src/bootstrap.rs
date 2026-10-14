//! Repository bootstrap (PLAN 6.5; P12).

use crate::history::History;
use scion_core::{ObjectId, Result};

impl History {
    /// Idempotently ensure the root changeset (empty tree) and ref `main` at version 1;
    /// returns the root changeset id.
    pub async fn bootstrap(&self) -> Result<ObjectId> {
        todo!("P12")
    }
}
