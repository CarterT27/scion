//! SPEC 8.2 / 18.3 deterministic rebase (P19).
//!
//! `Rebase(C, X)` with `P = C.parents[0]`, `B = tree(P)`, `O = tree(X)`, `T = tree(C)`:
//! only the paths of `C.delta` can differ between the result and `O`, so exactly those paths
//! and their ancestor directories in `B` and `O` are read (O(|delta| x depth) trees; no blob is
//! read unless a path needs a content merge). The result reuses `C`'s Meta and change id and
//! gets the recomputed delta against `O`; nothing time-dependent enters it, so identical
//! requests produce identical identifiers.

use crate::conflict::{PathInput, TreeReader, op_entry};
use crate::history::History;
use crate::hooks::Rebaser;
use crate::types::MetaSource;
use async_trait::async_trait;
use scion_core::{ErrorCode, ObjectId, Principal, Result, ScionError, subcode};
use scion_object::RepoPath;

impl History {
    /// SPEC 8.2 / 18.3: rebase `c` onto `onto`.
    ///
    /// - INVALID_ARGUMENT/multi_parent unless `c` has exactly one parent (v1).
    /// - Fast path: `onto == c.parents[0]` returns `c` itself.
    /// - Per delta path the SPEC 8.3 rules (structural `file_directory` first, then the table,
    ///   `redacted`, `binary` and the `line-diff3-v1` content merge). Every conflict is
    ///   collected and returned as MERGE_CONFLICT with a `ConflictReport` (`clean_entries` for
    ///   every non-conflicting delta path). Clean content merges are stored as new blobs;
    ///   conflict markers are never stored.
    /// - EMPTY_CHANGE when the recomputed delta against `tree(onto)` is empty.
    /// - Otherwise the changeset `{parents: [onto], meta: c.meta, change_id: c.change_id}` is
    ///   created through the internal CreateChangeset path. A pure function of
    ///   `(c, onto, merge engine version)`: no wall-clock value enters the result.
    pub async fn rebase(&self, c: &ObjectId, onto: &ObjectId, who: &Principal) -> Result<ObjectId> {
        let rec = self.changeset(c).await?;
        let cs = rec.changeset;
        let p = match cs.parents.as_slice() {
            [p] => *p,
            _ => {
                return Err(ScionError::invalid(
                    subcode::MULTI_PARENT,
                    format!(
                        "rebase requires exactly one parent; the changeset has {}",
                        cs.parents.len()
                    ),
                ));
            }
        };
        if *onto == p {
            return Ok(*c);
        }
        let x = self.changeset(onto).await?;
        let base = self.changeset(&p).await?;
        let (b_root, o_root) = (base.changeset.root_tree, x.changeset.root_tree);

        let mut reader = TreeReader::new(self);
        let paths: Vec<&RepoPath> = cs.delta.iter().map(|e| &e.path).collect();
        reader.prefetch(&[b_root, o_root], &paths).await?;
        let mut inputs = Vec::with_capacity(cs.delta.len());
        for e in &cs.delta {
            let t = op_entry(&e.op);
            let b = reader.probe(&b_root, &e.path).await?.file;
            let o_probe = reader.probe(&o_root, &e.path).await?;
            inputs.push(PathInput {
                path: e.path.clone(),
                b,
                o: o_probe.file,
                t,
                structural: o_probe.structural_conflict(t.as_ref()),
            });
        }
        drop(reader);

        let resolution = self.resolve_paths(inputs).await?;
        if let Some(err) = resolution.conflict_error("rebase") {
            return Err(err);
        }
        let delta = resolution.delta();
        if delta.is_empty() {
            return Err(ScionError::new(
                ErrorCode::EmptyChange,
                format!("the change of {c} is already present in {onto}"),
            ));
        }
        let created = self
            .create_internal(
                vec![*onto],
                delta,
                MetaSource::Existing(cs.meta),
                cs.change_id,
                who,
            )
            .await?;
        tracing::debug!(source = %c, onto = %onto, result = %created.changeset_id, "rebased");
        Ok(created.changeset_id)
    }
}

/// SPEC 8.4: Land rebases stack members through this seam; it is [`History::rebase`].
#[async_trait]
impl Rebaser for History {
    async fn rebase(&self, c: &ObjectId, onto: &ObjectId, who: &Principal) -> Result<ObjectId> {
        History::rebase(self, c, onto, who).await
    }
}
