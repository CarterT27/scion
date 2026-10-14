//! Repository bootstrap (PLAN 6.5; SPEC 4.3.4 root changeset; P12).

use crate::create::Spec;
use crate::history::History;
use crate::types::{MetaFields, MetaSource};
use scion_core::{ChangeId, ObjectId, Result, empty_tree_id};
use scion_object::{Extensions, RefName, Timestamp};
use scion_store::{CasOutcome, RefCas, RefExpect};

/// Author of the root changeset (PLAN 6.5).
const ROOT_AUTHOR: &[u8] = b"scion <scion@localhost>";
/// Message of the root changeset (PLAN 6.5).
const ROOT_MESSAGE: &[u8] = b"root";

/// The change id of the root changeset: `BLAKE3("scion-root-v1")[0..16]` (PLAN 6.5).
fn root_change_id() -> ChangeId {
    let digest = blake3::hash(b"scion-root-v1");
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest.as_bytes()[..16]);
    ChangeId(id)
}

/// The Meta fields of the root changeset (PLAN 6.5): author `scion <scion@localhost>`, time
/// 0/0, no committer, message `root`.
fn root_meta_fields() -> MetaFields {
    MetaFields {
        author: ROOT_AUTHOR.to_vec(),
        author_time: Timestamp {
            seconds: 0,
            tz_offset_minutes: 0,
        },
        committer: None,
        committer_time: None,
        message: ROOT_MESSAGE.to_vec(),
        extensions: Extensions::new(),
    }
}

impl History {
    /// Idempotently ensure the root changeset and ref `main` (PLAN 6.5, SPEC 4.3.4): the root
    /// changeset has no parents, an empty delta and the empty root tree; its Meta has author
    /// `scion <scion@localhost>`, time 0/0, no committer, message `root` and
    /// `creator = config.bootstrap_principal`; its change id is `BLAKE3("scion-root-v1")[0..16]`.
    /// Ref `main` is created at version 1 via `cas_ref(Absent)`; an existing `main` is left as
    /// it is. Returns the root changeset id.
    ///
    /// The SPEC 8.1 parentless rule applies: when a different parentless changeset already
    /// exists (for example the root of an earlier bootstrap with another
    /// `bootstrap_principal`), no second root is created and INVALID_ARGUMENT /
    /// parentless_not_allowed is returned (SPEC 4.3.4: exactly one root changeset).
    pub async fn bootstrap(&self) -> Result<ObjectId> {
        let who = self.cfg.bootstrap_principal.clone();
        let created = self
            .create_core(Spec {
                parents: Vec::new(),
                delta: Vec::new(),
                meta: MetaSource::New(root_meta_fields()),
                change_id: root_change_id(),
                expected_root_tree: Some(empty_tree_id()),
                import_mode: false,
                who: &who,
                authz: None,
                check_redacted: true,
            })
            .await?;
        let root = created.changeset_id;
        let outcome = self
            .meta()
            .cas_ref(RefCas {
                name: RefName::main(),
                expect: RefExpect::Absent,
                new_target: Some(root),
                principal: who.id_string(),
                at: self.clock.now(),
                landed: Vec::new(),
                audit: None,
            })
            .await?;
        match outcome {
            CasOutcome::Committed(_) => {
                tracing::info!(changeset = %root, "bootstrap created ref main");
            }
            CasOutcome::VersionMismatch { .. } | CasOutcome::LandedConflict { .. } => {
                tracing::debug!(changeset = %root, "bootstrap: ref main already exists");
            }
        }
        Ok(root)
    }
}
