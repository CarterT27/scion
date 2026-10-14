//! SPEC 8.3 `Merge(a, b)` two-parent merge changesets (P19).
//!
//! The base is the unique best common ancestor of `a` and `b` (the empty tree when there is
//! none, PLAN 6.14; AMBIGUOUS_MERGE_BASE when there are several, e.g. a criss-cross,
//! SPEC Appendix A). The examined paths are those of `FileDiff(base, a)` and
//! `FileDiff(base, b)`, both computed with identical-subtree pruning, and each is resolved by
//! the same per-path SPEC 8.3 rules as rebase with ours = `a` and theirs = `b`.

use crate::conflict::{PathInput, TreeReader};
use crate::history::History;
use crate::hooks::WriteAuthorizer;
use crate::types::{MetaFields, MetaSource};
use scion_core::{
    ChangeId, EntryInfo, ErrorCode, ObjectId, Principal, Result, ScionError, empty_tree_id,
};
use scion_object::RepoPath;
use std::collections::BTreeMap;

/// Base, ours and theirs entries of one examined path, and whether theirs changed it.
struct Sides {
    b: Option<EntryInfo>,
    o: Option<EntryInfo>,
    t: Option<EntryInfo>,
    theirs_changed: bool,
}

impl History {
    /// SPEC 8.3 `Merge(a, b)`: merge `b` into `a` over their unique merge base.
    ///
    /// - AMBIGUOUS_MERGE_BASE when more than one best common ancestor exists; no common
    ///   ancestor (unrelated imported roots) merges against the empty tree (PLAN 6.14).
    /// - Every path of `FileDiff(base, a)` and `FileDiff(base, b)` is resolved by the SPEC 8.3
    ///   rules (structural `file_directory` for paths `b` changed, the table, `redacted`,
    ///   `binary`, content merge); all conflicts are returned as MERGE_CONFLICT with a
    ///   `ConflictReport`.
    /// - PERMISSION_DENIED unless `authz.can_write` holds for every path of the resulting delta
    ///   against `a` (SPEC 12.4).
    /// - The result has parents `[a, b]`, a new Meta built from `meta` (creator = `who`), and
    ///   `change_id`; it is persisted by the CreateChangeset path (SPEC 5.3).
    pub async fn merge(
        &self,
        a: &ObjectId,
        b: &ObjectId,
        meta: MetaFields,
        change_id: ChangeId,
        who: &Principal,
        authz: &dyn WriteAuthorizer,
    ) -> Result<ObjectId> {
        let ra = self.changeset(a).await?;
        let rb = self.changeset(b).await?;
        let bases = self.merge_bases(a, b).await?;
        let base_root = match bases.as_slice() {
            [] => empty_tree_id(),
            [base] => self.changeset(base).await?.changeset.root_tree,
            many => {
                return Err(ScionError::new(
                    ErrorCode::AmbiguousMergeBase,
                    format!("{a} and {b} have {} best common ancestors", many.len()),
                ));
            }
        };
        let (o_root, t_root) = (ra.changeset.root_tree, rb.changeset.root_tree);

        let ours = self.file_diff(Some(&base_root), &o_root, None).await?;
        let theirs = self.file_diff(Some(&base_root), &t_root, None).await?;
        let mut sides: BTreeMap<RepoPath, Sides> = BTreeMap::new();
        for c in ours {
            sides.insert(
                c.path,
                Sides {
                    b: c.old,
                    o: c.new,
                    t: c.old,
                    theirs_changed: false,
                },
            );
        }
        for c in theirs {
            match sides.get_mut(&c.path) {
                Some(s) => {
                    s.t = c.new;
                    s.theirs_changed = true;
                }
                None => {
                    sides.insert(
                        c.path,
                        Sides {
                            b: c.old,
                            o: c.old,
                            t: c.new,
                            theirs_changed: true,
                        },
                    );
                }
            }
        }

        let mut reader = TreeReader::new(self);
        let mut inputs = Vec::with_capacity(sides.len());
        for (path, s) in sides {
            let structural = if s.theirs_changed {
                reader
                    .probe(&o_root, &path)
                    .await?
                    .structural_conflict(s.t.as_ref())
            } else {
                false
            };
            inputs.push(PathInput {
                path,
                b: s.b,
                o: s.o,
                t: s.t,
                structural,
            });
        }
        drop(reader);

        let resolution = self.resolve_paths(inputs).await?;
        if let Some(err) = resolution.conflict_error("merge") {
            return Err(err);
        }
        let delta = resolution.delta();
        if let Some(i) = delta.iter().position(|e| !authz.can_write(&e.path)) {
            return Err(ScionError::permission_denied(format!(
                "no write permission for merge delta entry {i}"
            )));
        }
        let created = self
            .create_internal(vec![*a, *b], delta, MetaSource::New(meta), change_id, who)
            .await?;
        tracing::debug!(ours = %a, theirs = %b, result = %created.changeset_id, "merged");
        Ok(created.changeset_id)
    }
}
