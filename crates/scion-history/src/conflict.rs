//! Per-path SPEC 8.3 conflict resolution shared by rebase and merge (P19).
//!
//! Both operations reduce to the same per-path problem: for every examined path `p` there is a
//! base entry `b`, an ours entry `o` (the tree the result is built on) and a theirs entry `t`,
//! plus the SPEC 8.3 structural rule evaluated against the ours tree `O`. Resolution runs in the
//! SPEC 18.3 order:
//!
//! 1. `file_directory` when theirs changed the path and `t` puts a file where `O` has a
//!    directory or a file at an ancestor, or deletes it while `O` has a directory there;
//! 2. the SPEC 8.3 table rows 1-6 ([`scion_merge::classify`]);
//! 3. for `NeedContent`: `redacted` when any side is quarantined or purged (SPEC 13.6; checked
//!    before `binary` because redacted bytes are unavailable), `binary` when any side is larger
//!    than `limits.merge_max_text_bytes` or contains `0x00` in its first 8000 bytes, otherwise
//!    the `line-diff3-v1` content merge. A clean merge is stored as a new blob with the mode
//!    from [`scion_merge::merge_mode`]; a conflicting one becomes a `content` conflict carrying
//!    its hunks.
//!
//! Conflict markers are never produced here: conflicts are returned as data (SPEC 8.3).

use crate::history::History;
use bytes::Bytes;
use scion_core::{
    CleanEntry, Conflict, ConflictHunk, ConflictKind, ConflictReport, EntryInfo, ErrorCode,
    ErrorPayload, Mode, ObjectId, Result, ScionError,
};
use scion_merge::{EntryDecision, Merge3, classify, is_binary, merge_mode, merge3};
use scion_object::{DeltaEntry, DeltaOp, RepoPath, Tree, TreeEntry};
use scion_store::{Consistency, ObjectState};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// SPEC 8.3 entry equality: both absent, or equal `(mode, id)` (the size is not compared).
pub(crate) fn same_entry(x: Option<&EntryInfo>, y: Option<&EntryInfo>) -> bool {
    match (x, y) {
        (None, None) => true,
        (Some(x), Some(y)) => x.mode == y.mode && x.id == y.id,
        _ => false,
    }
}

/// The file-level entry described by a delta operation (`None` for a delete).
pub(crate) fn op_entry(op: &DeltaOp) -> Option<EntryInfo> {
    match op {
        DeltaOp::Put { mode, id, size } => Some(EntryInfo {
            mode: *mode,
            id: *id,
            size: *size,
        }),
        DeltaOp::Delete => None,
    }
}

fn info(e: &TreeEntry) -> EntryInfo {
    EntryInfo {
        mode: e.mode,
        id: e.id,
        size: e.size,
    }
}

/// What a tree holds at one path (SPEC 8.3 structural rules).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Probe {
    /// The file entry at the path (modes 1-3).
    pub(crate) file: Option<EntryInfo>,
    /// A directory sits at the path.
    pub(crate) dir: bool,
    /// A file sits at a proper ancestor of the path.
    pub(crate) file_at_ancestor: bool,
}

impl Probe {
    /// SPEC 8.3 structural conflict for a path theirs changed to `t`: a Put where this tree has
    /// a directory at the path or a file at an ancestor, or a Delete where it has a directory.
    pub(crate) fn structural_conflict(&self, t: Option<&EntryInfo>) -> bool {
        match t {
            Some(_) => self.dir || self.file_at_ancestor,
            None => self.dir,
        }
    }
}

/// A per-operation memo of decoded trees, so each directory on the examined paths is read
/// once per operation (SPEC 8.2: O(|delta| x depth) tree reads).
pub(crate) struct TreeReader<'h> {
    h: &'h History,
    cache: HashMap<ObjectId, Arc<Tree>>,
}

impl<'h> TreeReader<'h> {
    pub(crate) fn new(h: &'h History) -> Self {
        TreeReader {
            h,
            cache: HashMap::new(),
        }
    }

    async fn tree(&mut self, id: &ObjectId) -> Result<Arc<Tree>> {
        if let Some(t) = self.cache.get(id) {
            return Ok(t.clone());
        }
        let t = self.h.tree(id).await?;
        self.cache.insert(*id, t.clone());
        Ok(t)
    }

    /// Walk `root` along `path`, reading only the directories on the way (SPEC 8.2).
    pub(crate) async fn probe(&mut self, root: &ObjectId, path: &RepoPath) -> Result<Probe> {
        let mut out = Probe::default();
        if path.is_root() {
            return Ok(out);
        }
        let mut current = *root;
        let mut comps = path.components().peekable();
        while let Some(name) = comps.next() {
            let tree = self.tree(&current).await?;
            let Some(entry) = tree.get(name) else {
                return Ok(out);
            };
            let last = comps.peek().is_none();
            match (last, entry.mode == Mode::Tree) {
                (true, true) => out.dir = true,
                (true, false) => out.file = Some(info(entry)),
                (false, true) => current = entry.id,
                (false, false) => {
                    out.file_at_ancestor = true;
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }
}

/// One examined path: base, ours and theirs entries plus the precomputed structural verdict.
#[derive(Clone, Debug)]
pub(crate) struct PathInput {
    pub(crate) path: RepoPath,
    pub(crate) b: Option<EntryInfo>,
    pub(crate) o: Option<EntryInfo>,
    pub(crate) t: Option<EntryInfo>,
    /// SPEC 8.3 structural `file_directory` conflict (theirs changed the path and `O`
    /// conflicts structurally with `t`).
    pub(crate) structural: bool,
}

/// The resolution of a set of paths (SPEC 18.3 `resolved` and `conflicts`).
#[derive(Clone, Debug, Default)]
pub(crate) struct Resolution {
    /// Every non-conflicting examined path with its resulting entry (`None` = absent).
    pub(crate) resolved: BTreeMap<RepoPath, Option<EntryInfo>>,
    /// The ours entry of each examined path (the delta is computed against it).
    pub(crate) ours: BTreeMap<RepoPath, Option<EntryInfo>>,
    /// Conflicts, ascending by path.
    pub(crate) conflicts: Vec<Conflict>,
}

impl Resolution {
    /// SPEC 18.3: the resolved entries that differ from `O`, ascending by path.
    pub(crate) fn delta(&self) -> Vec<DeltaEntry> {
        self.resolved
            .iter()
            .filter(|(p, e)| !same_entry(e.as_ref(), self.ours.get(*p).and_then(|o| o.as_ref())))
            .map(|(p, e)| DeltaEntry {
                path: p.clone(),
                op: match e {
                    Some(e) => DeltaOp::Put {
                        mode: e.mode,
                        id: e.id,
                        size: e.size,
                    },
                    None => DeltaOp::Delete,
                },
            })
            .collect()
    }

    /// SPEC 8.3 / 18.3: `MERGE_CONFLICT` with a [`ConflictReport`] (`clean_entries` for every
    /// non-conflicting examined path, the conflicts with base/ours/theirs entries and hunks),
    /// or `None` when nothing conflicts.
    pub(crate) fn conflict_error(&self, what: &str) -> Option<ScionError> {
        if self.conflicts.is_empty() {
            return None;
        }
        let report = ConflictReport {
            clean_entries: self
                .resolved
                .iter()
                .map(|(p, e)| CleanEntry {
                    path: p.as_bytes().to_vec(),
                    entry: *e,
                })
                .collect(),
            conflicts: self.conflicts.clone(),
        };
        tracing::debug!(
            conflicts = report.conflicts.len(),
            clean = report.clean_entries.len(),
            "{what}: merge conflict"
        );
        Some(
            ScionError::new(
                ErrorCode::MergeConflict,
                format!("{what}: {} conflicting path(s)", report.conflicts.len()),
            )
            .with_payload(ErrorPayload::MergeConflict(Box::new(report))),
        )
    }
}

/// Outcome of the content rows for one path.
enum Content {
    Clean(EntryInfo),
    Conflict(ConflictKind, Vec<ConflictHunk>),
}

impl History {
    /// Resolve every input path by SPEC 8.3 in the SPEC 18.3 order (see the module docs).
    /// Every conflict is collected; clean content merges are stored as blobs immediately so
    /// that `clean_entries` of a conflict report name stored objects.
    pub(crate) async fn resolve_paths(&self, inputs: Vec<PathInput>) -> Result<Resolution> {
        let mut out = Resolution::default();
        for input in inputs {
            let PathInput {
                path,
                b,
                o,
                t,
                structural,
            } = input;
            out.ours.insert(path.clone(), o);
            let conflict = |kind: ConflictKind, hunks: Vec<ConflictHunk>| Conflict {
                path: path.as_bytes().to_vec(),
                kind,
                base: b,
                ours: o,
                theirs: t,
                hunks,
            };
            if structural {
                out.conflicts
                    .push(conflict(ConflictKind::FileDirectory, Vec::new()));
                continue;
            }
            match classify(b.as_ref(), o.as_ref(), t.as_ref()) {
                EntryDecision::Take(e) => {
                    out.resolved.insert(path, e);
                }
                EntryDecision::Conflict(kind) => out.conflicts.push(conflict(kind, Vec::new())),
                EntryDecision::NeedContent => match (b, o, t) {
                    (Some(b), Some(o), Some(t)) => match self.merge_content(&b, &o, &t).await? {
                        Content::Clean(e) => {
                            out.resolved.insert(path, Some(e));
                        }
                        Content::Conflict(kind, hunks) => {
                            out.conflicts.push(conflict(kind, hunks));
                        }
                    },
                    // Not reachable after classify (rows 4 and 5 cover every absent side);
                    // reported as a conflict rather than guessed.
                    _ => out
                        .conflicts
                        .push(conflict(ConflictKind::ModifyDelete, Vec::new())),
                },
            }
        }
        Ok(out)
    }

    /// The SPEC 8.3 `redacted`, `binary` and content rows for three present file entries.
    async fn merge_content(&self, b: &EntryInfo, o: &EntryInfo, t: &EntryInfo) -> Result<Content> {
        let max_text = self.cfg.limits.merge_max_text_bytes;
        let mut ids: Vec<ObjectId> = vec![b.id, o.id, t.id];
        ids.sort();
        ids.dedup();

        // `redacted` (SPEC 13.6): redaction state comes from a strong read, never a stale one.
        let rows = self.meta().get_objects(&ids, Consistency::Strong).await?;
        for (id, row) in ids.iter().zip(&rows) {
            match row {
                Some(r) if r.state != ObjectState::Live => {
                    return Ok(Content::Conflict(ConflictKind::Redacted, Vec::new()));
                }
                Some(_) => {}
                None => {
                    return Err(ScionError::not_found(format!(
                        "merge input {id} does not exist"
                    )));
                }
            }
        }

        // `binary` by size, decided before any content is read.
        let too_big = |n: u64| n > max_text;
        if [b, o, t].iter().any(|e| too_big(e.size))
            || rows.iter().flatten().any(|r| too_big(r.size))
        {
            return Ok(Content::Conflict(ConflictKind::Binary, Vec::new()));
        }

        let mut content: BTreeMap<ObjectId, Bytes> = BTreeMap::new();
        for id in &ids {
            match self.objects.get(id).await {
                Ok(raw) => {
                    content.insert(*id, raw);
                }
                // Redacted between the state check and the read.
                Err(e) if e.code == ErrorCode::Redacted => {
                    return Ok(Content::Conflict(ConflictKind::Redacted, Vec::new()));
                }
                Err(e) => return Err(e),
            }
        }
        let get = |id: &ObjectId| content.get(id).map(|b| b.as_ref()).unwrap_or_default();
        let (cb, co, ct) = (get(&b.id), get(&o.id), get(&t.id));
        if [cb, co, ct]
            .iter()
            .any(|c| is_binary(c) || too_big(c.len() as u64))
        {
            return Ok(Content::Conflict(ConflictKind::Binary, Vec::new()));
        }

        match merge3(cb, co, ct) {
            Merge3::Clean(merged) => {
                let size = merged.len() as u64;
                let (id, _) = self.objects.put_blob(Bytes::from(merged)).await?;
                Ok(Content::Clean(EntryInfo {
                    mode: merge_mode(b.mode, o.mode, t.mode),
                    id,
                    size,
                }))
            }
            Merge3::Conflict(hunks) => Ok(Content::Conflict(ConflictKind::Content, hunks)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::Kind;

    fn e(mode: Mode, c: &str) -> EntryInfo {
        EntryInfo {
            mode,
            id: ObjectId::hash_blob(c.as_bytes()),
            size: c.len() as u64,
        }
    }

    #[test]
    fn entry_equality_ignores_size() {
        let a = e(Mode::File, "x");
        let mut b = a;
        b.size = 99;
        assert!(same_entry(Some(&a), Some(&b)));
        assert!(same_entry(None, None));
        assert!(!same_entry(Some(&a), None));
        let x = e(Mode::Executable, "x");
        assert!(!same_entry(Some(&a), Some(&x)));
    }

    #[test]
    fn structural_rules() {
        let f = e(Mode::File, "f");
        let dir = Probe {
            dir: true,
            ..Probe::default()
        };
        let anc = Probe {
            file_at_ancestor: true,
            ..Probe::default()
        };
        let plain = Probe {
            file: Some(f),
            ..Probe::default()
        };
        assert!(dir.structural_conflict(Some(&f)));
        assert!(dir.structural_conflict(None));
        assert!(anc.structural_conflict(Some(&f)));
        assert!(!anc.structural_conflict(None));
        assert!(!plain.structural_conflict(Some(&f)));
        assert!(!plain.structural_conflict(None));
    }

    #[test]
    fn delta_filters_entries_equal_to_ours() {
        let p = |s: &str| RepoPath::parse(s.as_bytes()).unwrap();
        let mut r = Resolution::default();
        let x = e(Mode::File, "x");
        let y = e(Mode::File, "y");
        r.resolved.insert(p("same"), Some(x));
        r.ours.insert(p("same"), Some(x));
        r.resolved.insert(p("gone"), None);
        r.ours.insert(p("gone"), Some(x));
        r.resolved.insert(p("absent"), None);
        r.ours.insert(p("absent"), None);
        r.resolved.insert(p("new"), Some(y));
        r.ours.insert(p("new"), None);
        let d = r.delta();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].path, p("gone"));
        assert_eq!(d[0].op, DeltaOp::Delete);
        assert_eq!(d[1].path, p("new"));
        assert!(r.conflict_error("t").is_none());
        assert_eq!(x.id.kind(), Kind::Blob);
    }
}
