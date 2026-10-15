//! SPEC 8.1 / 18.1 CreateChangeset with SPEC 5.3 write ordering (P12).
//!
//! Validation runs in the exact SPEC 8.1 order and the first failure is returned:
//!
//! 1. parents exist (missing_parent), are unique (duplicate_parent) and at most
//!    `limits.max_parents` (TOO_LARGE); empty parents only while the repository has no root
//!    changeset (parentless_not_allowed), or in import mode with `can_import()`
//!    (PERMISSION_DENIED otherwise);
//! 2. delta shape: sorted, unique, `limits.max_delta_entries`, path rules;
//! 3. every Put blob exists (missing_blob) with the stored size (size_mismatch); with client
//!    authorization, a blob the caller neither uploaded nor can read at some `object_paths` row
//!    is reported missing_blob too, so referencing it at a readable path cannot leak it and the
//!    error reveals neither its existence nor its size (SPEC 12.5);
//! 4. no Put references a quarantined or purged blob (REDACTED); entries carried over unchanged
//!    from the first parent are not in the delta and need no check (SPEC 13.6);
//! 5. and 6. deletes and puts against `T1` via SPEC 18.2 apply_delta (delete_absent,
//!    delete_not_file, noop_put, file_directory);
//! 7. `can_write` on every delta path (PERMISSION_DENIED);
//! 8. Meta fields within `limits.max_message_bytes` (TOO_LARGE).
//!
//! Then `expected_root_tree` (root_tree_mismatch), the Meta (creator = the caller's principal
//! id string, never client supplied) and the identifier. Persistence follows SPEC 5.3 step 2
//! with fault checkpoints between the steps: new trees, then the Meta, then eager derivation,
//! then ONE `commit_changeset` transaction carrying the changesets row (generation and skip
//! pointers, SPEC 9.2), the children rows, the `object_paths` rows (`path_index`) and the eager
//! derived rows. Every write is idempotent, so identical requests return the same identifier
//! and never error, sequentially or concurrently.

use crate::history::History;
use crate::hooks::{EagerInput, WriteAuthorizer};
use crate::types::{Applied, CreateRequest, Created, MetaFields, MetaSource};
use crate::{compute_commit_graph, compute_fp_merges};
use bytes::Bytes;
use scion_core::{
    ChangeId, ErrorCode, Kind, ObjectId, Principal, Result, ScionError, empty_tree_id, points,
    subcode,
};
use scion_object::{Canonical, Changeset, DeltaEntry, DeltaOp, Extensions, Meta, RepoPath, Tree};
use scion_store::{
    ChangesetBundle, ChangesetRow, Consistency, ObjectPathRow, ObjectRow, ObjectState,
};
use std::collections::{BTreeMap, BTreeSet};

/// The most `object_paths` rows read when deciding whether a Put may reference a blob.
const MAX_REFERENCE_PATH_ROWS: usize = 100_000;

/// One creation, client facing (`authz` set) or internal (`authz` = `None`).
pub(crate) struct Spec<'a> {
    pub(crate) parents: Vec<ObjectId>,
    pub(crate) delta: Vec<DeltaEntry>,
    pub(crate) meta: MetaSource,
    pub(crate) change_id: ChangeId,
    pub(crate) expected_root_tree: Option<ObjectId>,
    pub(crate) import_mode: bool,
    pub(crate) who: &'a Principal,
    /// Client-facing authorization; `None` skips SPEC 8.1 step 7 (rebase, merge, bootstrap).
    pub(crate) authz: Option<&'a dyn WriteAuthorizer>,
    /// SPEC 8.1 step 4 (REDACTED for redacted Put blobs).
    pub(crate) check_redacted: bool,
}

/// A validated changeset with everything persistence needs (SPEC 8.1 done, nothing written).
struct Prepared {
    id: ObjectId,
    encoded: Bytes,
    changeset: Changeset,
    applied: Applied,
    t1: ObjectId,
    parent_rows: Vec<ChangesetRow>,
    meta_id: ObjectId,
    meta_obj: Meta,
    /// Canonical bytes of a new Meta (`None` for an existing one).
    meta_bytes: Option<Bytes>,
    import_mode: bool,
    created_by: String,
}

impl Prepared {
    fn created(&self) -> Created {
        Created {
            changeset_id: self.id,
            root_tree: self.applied.root,
            meta_id: self.meta_id,
        }
    }
}

fn missing_parent(i: usize) -> ScionError {
    ScionError::invalid(
        subcode::MISSING_PARENT,
        format!("parent {i} does not exist"),
    )
}

/// SPEC 8.1 step 8: the Meta message against `max_message_bytes` (TOO_LARGE).
fn check_meta_len(limit: u64, len: usize) -> Result<()> {
    let actual = len as u64;
    if actual > limit {
        return Err(ScionError::too_large("max_message_bytes", limit, actual));
    }
    Ok(())
}

/// The Meta of `fields` with the server-set `creator` (SPEC 4.3.3, 7.5).
fn build_meta(fields: MetaFields, who: &Principal) -> Meta {
    Meta {
        author: fields.author,
        author_time: fields.author_time,
        committer: fields.committer,
        committer_time: fields.committer_time,
        creator: who.id_string().into_bytes(),
        message: fields.message,
        extensions: fields.extensions,
    }
}

impl History {
    /// SPEC 8.1 / 18.1 CreateChangeset: validate in the exact SPEC 8.1 order (first failure
    /// returned), build the trees (SPEC 18.2) and the Meta with `creator = who.id_string()`, and
    /// persist per SPEC 5.3. Identical requests return the same id and never error.
    ///
    /// `import_mode` (PLAN 6.4) requires `authz.can_import()` and permits a parentless
    /// changeset after the root exists; it is passed to eager derivation (`git_mapping`).
    pub async fn create_changeset(
        &self,
        req: CreateRequest,
        who: &Principal,
        authz: &dyn WriteAuthorizer,
    ) -> Result<Created> {
        self.create_core(Spec {
            parents: req.parents,
            delta: req.delta,
            meta: MetaSource::New(req.meta),
            change_id: req.change_id,
            expected_root_tree: req.expected_root_tree,
            import_mode: req.import_mode,
            who,
            authz: Some(authz),
            check_redacted: true,
        })
        .await
    }

    /// Internal creation used by rebase and merge (SPEC 8.2, 8.3): the SPEC 8.1 validation
    /// without client-facing authorization (step 7) and without the redacted-Put check (step 4:
    /// rebase and merge decide about redacted content themselves, SPEC 13.6); `meta` may name an
    /// existing Meta object (rebase reuses the original Meta, which may itself be redacted).
    pub async fn create_internal(
        &self,
        parents: Vec<ObjectId>,
        delta: Vec<DeltaEntry>,
        meta: MetaSource,
        change_id: ChangeId,
        who: &Principal,
    ) -> Result<Created> {
        self.create_core(Spec {
            parents,
            delta,
            meta,
            change_id,
            expected_root_tree: None,
            import_mode: false,
            who,
            authz: None,
            check_redacted: false,
        })
        .await
    }

    /// The shared SPEC 8.1 + 5.3 implementation.
    ///
    /// A parentless request after the root exists fails parentless_not_allowed (step 1), except
    /// when the identical changeset already exists: SPEC 8.1 requires an identical retry (for
    /// example after a crash past the commit point, SPEC 16.1 class 4) to return the same id.
    /// The remaining validation still runs first, so any request that is not an exact retry
    /// reports the step-1 failure.
    pub(crate) async fn create_core(&self, spec: Spec<'_>) -> Result<Created> {
        let mut parentless_denied = false;
        let prepared = self.prepare(spec, &mut parentless_denied).await;
        if parentless_denied {
            match &prepared {
                Ok(p) if self.objects.stat(&p.id).await?.is_some() => {
                    tracing::debug!(changeset = %p.id, "identical parentless changeset exists");
                    return Ok(p.created());
                }
                // A transient failure must not turn into a permanent validation error.
                Err(e) if e.retryable() => return Err(e.clone()),
                _ => {}
            }
            return Err(ScionError::invalid(
                subcode::PARENTLESS_NOT_ALLOWED,
                "a parentless changeset is allowed only for the root or in import mode",
            ));
        }
        self.persist(prepared?).await
    }

    /// Per id: may `who` reference this blob in a Put (SPEC 12.5)? True when they uploaded it or
    /// one of its `object_paths` rows is readable; otherwise the new row would make an
    /// unreadable blob readable at the Put's path. Absent blobs are true (step 3 reports them).
    async fn referenceable(
        &self,
        authz: &dyn WriteAuthorizer,
        who: &Principal,
        ids: &[ObjectId],
        rows: &[Option<ObjectRow>],
    ) -> Result<Vec<bool>> {
        let uploaded = self.meta().has_uploads(ids, &who.id_string()).await?;
        let mut out = Vec::with_capacity(ids.len());
        for ((id, row), uploaded) in ids.iter().zip(rows).zip(uploaded) {
            let ok = uploaded
                || row.is_none()
                || self
                    .meta()
                    .object_paths(id, MAX_REFERENCE_PATH_ROWS)
                    .await?
                    .iter()
                    .any(|r| authz.can_read(&r.path));
            out.push(ok);
        }
        Ok(out)
    }

    /// SPEC 8.1 validation, tree construction (SPEC 18.2), the Meta and the identifier; no
    /// writes. A violated parentless rule sets `parentless_denied` instead of failing.
    async fn prepare(&self, spec: Spec<'_>, parentless_denied: &mut bool) -> Result<Prepared> {
        let limits = &self.cfg.limits;
        let Spec {
            parents,
            delta,
            meta,
            change_id,
            expected_root_tree,
            import_mode,
            who,
            authz,
            check_redacted,
        } = spec;

        // 1. Parents: exist, unique, count; the parentless rule.
        if !parents.is_empty() {
            if let Some(i) = parents.iter().position(|p| p.kind() != Kind::Changeset) {
                return Err(missing_parent(i));
            }
            let stats = self.objects.stat_many(&parents).await?;
            if let Some(i) = stats.iter().position(Option::is_none) {
                return Err(missing_parent(i));
            }
            let mut seen = BTreeSet::new();
            if let Some(i) = parents.iter().position(|p| !seen.insert(*p)) {
                return Err(ScionError::invalid(
                    subcode::DUPLICATE_PARENT,
                    format!("parent {i} repeats an earlier parent"),
                ));
            }
            let n = parents.len() as u64;
            if n > limits.max_parents {
                return Err(ScionError::too_large("max_parents", limits.max_parents, n));
            }
        } else if import_mode {
            if !authz.is_some_and(|a| a.can_import()) {
                return Err(ScionError::permission_denied(
                    "import mode requires admin.refs and write on /",
                ));
            }
        } else if self.meta().has_root_changeset().await? {
            *parentless_denied = true;
        }

        // 2. Delta shape (sorted, unique, count, path rules) via the shared shape check.
        let mut shape = Changeset {
            parents: Vec::new(),
            root_tree: empty_tree_id(),
            meta: ObjectId::new(Kind::Meta, [0; 32]),
            change_id,
            delta,
            extensions: Extensions::new(),
        };
        shape.validate_shape(limits)?;
        let delta = std::mem::take(&mut shape.delta);

        // 3. and 4. Put blobs: exist, size, not redacted (one strong batch read: redaction
        // state must not come from a stale snapshot, SPEC 5.4, 13.6).
        let puts: Vec<(usize, ObjectId, u64)> = delta
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match &e.op {
                DeltaOp::Put { id, size, .. } => Some((i, *id, *size)),
                DeltaOp::Delete => None,
            })
            .collect();
        if !puts.is_empty() {
            let ids: Vec<ObjectId> = puts.iter().map(|p| p.1).collect();
            let rows = self.meta().get_objects(&ids, Consistency::Strong).await?;
            let referenceable = match authz {
                Some(a) if a.checks_blob_access() => {
                    self.referenceable(a, who, &ids, &rows).await?
                }
                _ => vec![true; ids.len()],
            };
            for (((i, _, size), row), ok) in puts.iter().zip(&rows).zip(referenceable) {
                match row {
                    Some(_) if !ok => {
                        return Err(ScionError::invalid(
                            subcode::MISSING_BLOB,
                            format!("delta entry {i}: blob does not exist"),
                        ));
                    }
                    None => {
                        return Err(ScionError::invalid(
                            subcode::MISSING_BLOB,
                            format!("delta entry {i}: blob does not exist"),
                        ));
                    }
                    Some(r) if r.size != *size => {
                        return Err(ScionError::invalid(
                            subcode::SIZE_MISMATCH,
                            format!(
                                "delta entry {i}: size {size} differs from the stored size {}",
                                r.size
                            ),
                        ));
                    }
                    Some(_) => {}
                }
            }
            if check_redacted {
                for ((_, id, _), row) in puts.iter().zip(&rows) {
                    if row.as_ref().is_some_and(|r| r.state != ObjectState::Live) {
                        return Err(ScionError::redacted(id));
                    }
                }
            }
        }

        // Parent rows (validated above; at most max_parents) and T1.
        let mut parent_rows: Vec<ChangesetRow> = Vec::with_capacity(parents.len());
        let mut t1 = empty_tree_id();
        for (i, p) in parents.iter().enumerate() {
            let (cs, row) = match self.objects.get_changeset(p).await {
                Ok(v) => v,
                Err(e) if e.code == ErrorCode::NotFound => return Err(missing_parent(i)),
                Err(e) => return Err(e),
            };
            if i == 0 {
                t1 = cs.root_tree;
            }
            parent_rows.push(row);
        }

        // 5. and 6. Deletes and puts against T1 (SPEC 18.2; delta relative to parents[0]).
        let applied = self.apply_delta(&t1, &delta).await?;

        // 7. Write authorization on every delta path.
        if let Some(authz) = authz {
            if import_mode && !authz.can_import() {
                return Err(ScionError::permission_denied(
                    "import mode requires admin.refs and write on /",
                ));
            }
            if let Some(i) = delta.iter().position(|e| !authz.can_write(&e.path)) {
                return Err(ScionError::permission_denied(format!(
                    "no write permission for delta entry {i}"
                )));
            }
        }

        // 8. Meta fields within limits; build the Meta (creator set by the server).
        let (meta_id, meta_obj, meta_bytes) = match meta {
            MetaSource::New(fields) => {
                let limit = limits.max_message_bytes;
                check_meta_len(limit, fields.message.len())?;
                let m = build_meta(fields, who);
                let bytes = m.encode();
                // Rejects e.g. unknown critical (odd) extension tags before anything is stored.
                Meta::decode(&bytes)?;
                let id = ObjectId::hash_encoded(Kind::Meta, &bytes);
                (id, m, Some(Bytes::from(bytes)))
            }
            MetaSource::Existing(id) => {
                if id.kind() != Kind::Meta {
                    return Err(ScionError::invalid(
                        subcode::INVALID_KIND,
                        format!("{id} is not a Meta id"),
                    ));
                }
                let m = match self.objects.get_meta(&id).await {
                    Ok(m) => (*m).clone(),
                    // A redacted Meta keeps its row (SPEC 13.1); its fields are not needed.
                    Err(e) if e.code == ErrorCode::Redacted => Meta {
                        author: Vec::new(),
                        author_time: scion_object::Timestamp {
                            seconds: 0,
                            tz_offset_minutes: 0,
                        },
                        committer: None,
                        committer_time: None,
                        creator: Vec::new(),
                        message: Vec::new(),
                        extensions: Extensions::new(),
                    },
                    Err(e) => return Err(e),
                };
                (id, m, None)
            }
        };

        if let Some(expected) = expected_root_tree
            && expected != applied.root
        {
            return Err(ScionError::invalid(
                subcode::ROOT_TREE_MISMATCH,
                format!(
                    "result root tree {} differs from the expected {expected}",
                    applied.root
                ),
            ));
        }

        let changeset = Changeset {
            parents,
            root_tree: applied.root,
            meta: meta_id,
            change_id,
            delta,
            extensions: Extensions::new(),
        };
        let encoded = Bytes::from(changeset.encode());
        let id = ObjectId::hash_encoded(Kind::Changeset, &encoded);
        Ok(Prepared {
            id,
            encoded,
            changeset,
            applied,
            t1,
            parent_rows,
            meta_id,
            meta_obj,
            meta_bytes,
            import_mode,
            created_by: who.id_string(),
        })
    }

    /// SPEC 5.3 step 2 persistence of a prepared changeset, with the create.* checkpoints.
    async fn persist(&self, prepared: Prepared) -> Result<Created> {
        let Prepared {
            id,
            encoded,
            changeset,
            applied,
            t1,
            parent_rows,
            meta_id,
            meta_obj,
            meta_bytes,
            import_mode,
            created_by,
        } = prepared;
        let faults = self.objects.storage().faults.clone();
        faults.check(points::CREATE_AFTER_VALIDATE).await?;

        // SPEC 5.3 step 2: write new trees (an empty root is the empty tree object), in one
        // existence read and one insert.
        // The trees come from apply_delta, which encodes and hashes them, so they are not
        // decoded or hashed again.
        let mut trees: Vec<(ObjectId, Bytes)> = applied
            .new_trees
            .iter()
            .map(|(_, id, b)| (*id, b.clone()))
            .collect();
        if applied.root == empty_tree_id() {
            trees.push((empty_tree_id(), Bytes::from(Tree::empty().encode())));
        }
        self.objects
            .put_encoded_many_prehashed(Kind::Tree, trees)
            .await?;
        faults.check(points::CREATE_AFTER_TREES).await?;
        if let Some(bytes) = meta_bytes {
            self.objects.put_encoded(Kind::Meta, bytes).await?;
        }
        faults.check(points::CREATE_AFTER_META).await?;

        // New trees with their directory paths: every tree of the result that differs from T1
        // at its path (an emptied root included).
        let mut new_trees: Vec<(RepoPath, ObjectId)> =
            Vec::with_capacity(applied.new_trees.len() + 1);
        if applied.root == empty_tree_id() && applied.root != t1 {
            new_trees.push((RepoPath::root(), applied.root));
        }
        new_trees.extend(applied.new_trees.iter().map(|(p, t, _)| (p.clone(), *t)));

        // Eager derived data, computed before the row exists (SPEC 9.1).
        let derived = self
            .eager
            .derive_eager(&EagerInput {
                id: &id,
                changeset: &changeset,
                meta: &meta_obj,
                new_trees: &new_trees,
                import_mode,
            })
            .await?;
        faults.check(points::CREATE_AFTER_DERIVE).await?;

        // commit_graph (SPEC 9.2): prefetch the first-parent skip chain, then compute.
        let mut rows: BTreeMap<ObjectId, ChangesetRow> =
            parent_rows.iter().map(|r| (r.id, r.clone())).collect();
        if let Some(fp) = parent_rows.first() {
            let mut next = fp.skip.first().copied();
            let mut k = 2usize;
            while let Some(c) = next {
                let row = match rows.get(&c) {
                    Some(r) => r.clone(),
                    None => {
                        let (_, r) = self.objects.get_changeset(&c).await?;
                        rows.insert(c, r.clone());
                        r
                    }
                };
                next = row.skip.get(k - 1).copied();
                k += 1;
            }
        }
        let (generation, skip) = compute_commit_graph(parent_rows.first(), &parent_rows, |c, j| {
            rows.get(c).and_then(|r| r.skip.get(j).copied())
        });
        let fp_merges = compute_fp_merges(parent_rows.first(), parent_rows.len());

        // path_index (SPEC 9.2): every Put blob at its path, every new tree at its directory.
        let now = self.clock.now();
        let mut object_paths: Vec<ObjectPathRow> = changeset
            .delta
            .iter()
            .filter_map(|e| match &e.op {
                DeltaOp::Put { id: blob, .. } => Some(ObjectPathRow {
                    object: *blob,
                    path: e.path.clone(),
                    first_changeset: id,
                    at: now,
                }),
                DeltaOp::Delete => None,
            })
            .collect();
        object_paths.extend(new_trees.iter().map(|(p, t)| ObjectPathRow {
            object: *t,
            path: p.clone(),
            first_changeset: id,
            at: now,
        }));
        // An empty root tree also unchanged from T1 (the root changeset, or an empty change on
        // an empty parent) still gets its row at the root path: tree reads are authorized
        // through `object_paths` and the root tree sits at the empty path (implementation-defined
        // 6.12). ON CONFLICT DO NOTHING makes a repeated row harmless.
        if applied.root == empty_tree_id() && applied.root == t1 {
            object_paths.push(ObjectPathRow {
                object: applied.root,
                path: RepoPath::root(),
                first_changeset: id,
                at: now,
            });
        }

        let bundle = ChangesetBundle {
            row: ChangesetRow {
                id,
                encoded,
                generation,
                skip,
                fp_merges,
                change_id: changeset.change_id,
                created_by,
                created_at: now,
            },
            parents: changeset.parents.clone(),
            object_paths,
            derived,
        };
        let outcome = self.meta().commit_changeset(bundle).await?;
        faults.check(points::CREATE_AFTER_COMMIT).await?;
        tracing::debug!(changeset = %id, ?outcome, generation, "changeset committed");
        Ok(Created {
            changeset_id: id,
            root_tree: applied.root,
            meta_id,
        })
    }
}
