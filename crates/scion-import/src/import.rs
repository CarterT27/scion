//! The import walk (SPEC 14; PLAN 6.4; P29).
//!
//! 1. Resolve the git ref with gix and list every reachable commit, parents first.
//! 2. Find the commits already imported: walking down from the tip, a commit whose
//!    `Resolve("git:<sha>")` succeeds is imported, and so are all its ancestors (CreateChangeset
//!    requires existing parents), so the walk stops there. Re-imports are idempotent and
//!    incremental imports touch only new commits.
//! 3. For each new commit, in topological order: upload the blobs the server lacks
//!    (HasObjects, then UploadObjects in batches of up to `batch_bytes`), compute the delta
//!    against the first parent's git tree mapped to Scion modes, and CreateChangeset in
//!    import mode with the mapped parents. The server derives `git_mapping` eagerly.
//! 4. Create the target ref, or fast-forward it with `expected_version`.

mod git;
mod rpc;

pub(crate) use git::{CommitInfo, GitOid, GitRepo, oid_bytes};

use crate::mapping::{change_id_for, encode_skipped_gitlinks, parse_commit_header};
use bytes::Bytes;
use git::Change;
use rpc::Rpc;
use scion_core::{ErrorCode, Kind, ObjectId, Result, ScionError, empty_tree_id};
use scion_object::{
    Canonical, Changeset, DeltaEntry, DeltaOp, Extensions, META_EXT_GIT_COMMIT,
    META_EXT_SKIPPED_GITLINKS, RefName, RepoPath,
};
use scion_proto::convert::{CreateChangesetParts, MetaFieldsParts, id_bytes, parse_id};
use scion_proto::{ScionClient, v1};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

/// Ids per HasObjects request.
const HAS_OBJECTS_CHUNK: usize = 1000;

/// Import options (SPEC 14 CLI flags).
#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// The git repository.
    pub git_dir: PathBuf,
    /// The git ref to import.
    pub git_ref: String,
    /// The Scion ref to create or fast-forward.
    pub target_ref: RefName,
    /// Omit gitlinks (recorded in Meta extension tag 4) instead of failing.
    pub skip_submodules: bool,
    /// Upload batch size in bytes.
    pub batch_bytes: usize,
}

/// Import summary (SPEC 14).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct ImportReport {
    /// Git commits reachable from the ref.
    pub commits_seen: u64,
    /// Changesets created.
    pub imported: u64,
    /// Commits already imported (incremental re-import).
    pub skipped_existing: u64,
    /// Blobs uploaded.
    pub blobs_uploaded: u64,
    /// Skipped gitlinks as `(path, sha hex)`.
    pub skipped_gitlinks: Vec<(String, String)>,
    /// The imported tip.
    pub final_target: Option<ObjectId>,
    /// Version of the target ref after the import.
    pub ref_version: u64,
}

/// Import `opts.git_ref` into `opts.target_ref` through `client` (SPEC 14).
///
/// The client must authenticate as a principal with write on `/` and `admin.refs`
/// (implementation-defined 6.4). Every git commit reachable from the ref becomes one
/// changeset created in import mode: parents are the mapped parents (octopus merges keep all
/// of them; root commits become parentless changesets), the delta is the FileDiff from the
/// first parent's tree, and the Meta carries author, committer, both timestamps and zones,
/// the message verbatim, extension tag 2 (the 20-byte commit id) and, with
/// `skip_submodules`, extension tag 4 (the commit tree's gitlinks). Without
/// `skip_submodules` a gitlink fails the import with INVALID_ARGUMENT.
///
/// After the walk the target ref is created (CreateRef) when absent, or fast-forwarded
/// (UpdateRef with `expected_version`). A ref still at the repository's bootstrap root
/// changeset (implementation-defined 6.5) is replaced by a forced update, because imported
/// histories start at their own parentless changesets; this needs `force` on the ref. A ref
/// that already descends from the imported tip is left unchanged. Any other
/// non-fast-forward is NOT_FAST_FORWARD. A git commit listing the same parent twice imports
/// with that parent once (changeset parents are unique, SPEC 4.3.4). RESOURCE_EXHAUSTED answers (rate limiting)
/// are retried with back-off.
pub async fn import(client: &ScionClient, opts: &ImportOptions) -> Result<ImportReport> {
    let git = GitRepo::open(&opts.git_dir)?;
    let mut imp = Importer {
        rpc: Rpc { client },
        opts,
        git,
        blob_ids: HashMap::new(),
        present: HashSet::new(),
        mapped: HashMap::new(),
        report: ImportReport::default(),
        skipped_links: BTreeSet::new(),
    };
    imp.run().await
}

struct Importer<'a> {
    rpc: Rpc<'a>,
    opts: &'a ImportOptions,
    git: GitRepo,
    /// git blob id -> (Scion blob id, size).
    blob_ids: HashMap<GitOid, (ObjectId, u64)>,
    /// Blob ids known to be on the server.
    present: HashSet<ObjectId>,
    /// git commit id -> imported changeset id.
    mapped: HashMap<GitOid, ObjectId>,
    report: ImportReport,
    skipped_links: BTreeSet<(String, String)>,
}

fn with_commit(e: ScionError, id: &GitOid) -> ScionError {
    let message = format!("git commit {id}: {}", e.message);
    ScionError {
        code: e.code,
        message,
        payload: e.payload,
    }
}

fn internal(msg: impl Into<String>) -> ScionError {
    ScionError::new(ErrorCode::Unavailable, msg)
}

/// `BLAKE3("scion-root-v1")[0..16]`: the bootstrap root's change id (implementation-defined
/// 6.5).
fn bootstrap_root_change_id() -> [u8; 16] {
    let d = blake3::hash(b"scion-root-v1");
    let mut out = [0u8; 16];
    out.copy_from_slice(&d.as_bytes()[..16]);
    out
}

impl Importer<'_> {
    async fn run(&mut self) -> Result<ImportReport> {
        let tip = self.git.resolve_commit(&self.opts.git_ref)?;
        let commits = self.git.topo_order(tip)?;
        self.report.commits_seen = commits.len() as u64;
        let by_id: HashMap<GitOid, usize> =
            commits.iter().enumerate().map(|(i, c)| (c.id, i)).collect();

        // Which commits are new: walk down from the tip, stopping at imported commits.
        let mut need: HashSet<GitOid> = HashSet::new();
        let mut seen: HashSet<GitOid> = HashSet::new();
        let mut stack = vec![tip];
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            if let Some(cs) = self.rpc.resolve_git(&id.to_string()).await? {
                self.mapped.insert(id, cs);
                continue;
            }
            need.insert(id);
            if let Some(c) = by_id.get(&id).and_then(|&i| commits.get(i)) {
                stack.extend(c.parents.iter().copied());
            }
        }
        tracing::info!(
            commits_seen = self.report.commits_seen,
            to_import = need.len(),
            "scion-import: walk planned"
        );

        for c in &commits {
            if !need.contains(&c.id) {
                continue;
            }
            let parent_tree = match c.parents.first() {
                Some(p) => Some(
                    by_id
                        .get(p)
                        .and_then(|&i| commits.get(i))
                        .map(|pc| pc.tree)
                        .ok_or_else(|| internal(format!("parent {p} was not walked")))?,
                ),
                None => None,
            };
            let cs = self
                .import_commit(c, parent_tree)
                .await
                .map_err(|e| with_commit(e, &c.id))?;
            self.mapped.insert(c.id, cs);
            self.report.imported += 1;
            if self.report.imported.is_multiple_of(100) {
                tracing::info!(imported = self.report.imported, "scion-import: progress");
            }
        }
        self.report.skipped_existing = self.report.commits_seen - self.report.imported;
        self.report.skipped_gitlinks = std::mem::take(&mut self.skipped_links)
            .into_iter()
            .collect();

        let tip_cs = self
            .mapped
            .get(&tip)
            .copied()
            .ok_or_else(|| internal("the tip commit was not imported"))?;
        self.report.final_target = Some(tip_cs);
        self.report.ref_version = self.update_target(tip_cs, &tip).await?;
        tracing::info!(
            imported = self.report.imported,
            skipped_existing = self.report.skipped_existing,
            blobs_uploaded = self.report.blobs_uploaded,
            ref_version = self.report.ref_version,
            "scion-import: done"
        );
        Ok(self.report.clone())
    }

    /// Import one commit whose parents are all mapped; returns the changeset id.
    async fn import_commit(
        &mut self,
        c: &CommitInfo,
        parent_tree: Option<GitOid>,
    ) -> Result<ObjectId> {
        let header = parse_commit_header(&c.raw)?;
        let links = self.git.gitlinks(c.tree)?;
        if let Some((path, _)) = links.first() {
            if !self.opts.skip_submodules {
                return Err(ScionError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "gitlink (submodule) at {:?}; rerun with --skip-submodules to omit it",
                        String::from_utf8_lossy(path)
                    ),
                ));
            }
            for (path, sha) in links.iter() {
                self.skipped_links
                    .insert((String::from_utf8_lossy(path).into_owned(), hex_lower(sha)));
            }
        }

        let changes = self.git.diff_trees(parent_tree, Some(c.tree))?;
        let mut pending: BTreeMap<ObjectId, Bytes> = BTreeMap::new();
        let mut delta = Vec::with_capacity(changes.len());
        for (path, change) in changes {
            let rp = RepoPath::parse(&path).map_err(|e| {
                ScionError::new(
                    e.code,
                    format!(
                        "path {:?} is not a valid Scion path: {}",
                        String::from_utf8_lossy(&path),
                        e.message
                    ),
                )
                .with_payload(e.payload.clone())
            })?;
            let op = match change {
                Change::Delete => DeltaOp::Delete,
                Change::Put { mode, oid } => {
                    let (id, size) = self.blob_id(oid, &mut pending)?;
                    DeltaOp::Put { mode, id, size }
                }
            };
            delta.push(DeltaEntry { path: rp, op });
        }
        self.ensure_uploaded(pending).await?;

        let sha = oid_bytes(&c.id)?;
        let mut extensions = Extensions::new();
        extensions.insert(META_EXT_GIT_COMMIT, sha.to_vec())?;
        if !links.is_empty() {
            extensions.insert(META_EXT_SKIPPED_GITLINKS, encode_skipped_gitlinks(&links))?;
        }
        let parents = self.mapped_parents(c)?;
        let parts = CreateChangesetParts {
            parents,
            delta,
            meta: MetaFieldsParts {
                author: header.author.ident,
                author_time: header.author.time,
                committer: Some(header.committer.ident),
                committer_time: Some(header.committer.time),
                message: header.message,
                extensions,
            },
            change_id: change_id_for(&sha),
            expected_root_tree: None,
            import_mode: true,
        };
        self.rpc
            .create_changeset(v1::CreateChangesetRequest::from(&parts))
            .await
    }

    /// The mapped Scion parents of `c`, in git order. Git tolerates a commit listing the same
    /// parent twice (old histories contain such commits, e.g. written by early `git
    /// commit-tree`), but changeset parents are unique (SPEC 4.3.4): repeats are dropped,
    /// keeping the first occurrence, and the commit graph is otherwise unchanged.
    fn mapped_parents(&self, c: &CommitInfo) -> Result<Vec<ObjectId>> {
        let mut parents: Vec<ObjectId> = Vec::with_capacity(c.parents.len());
        for p in &c.parents {
            let id = self
                .mapped
                .get(p)
                .copied()
                .ok_or_else(|| internal(format!("parent {p} is not imported")))?;
            if parents.contains(&id) {
                tracing::warn!(commit = %c.id, parent = %p, "scion-import: duplicate git parent dropped");
            } else {
                parents.push(id);
            }
        }
        Ok(parents)
    }

    /// The Scion id and size of git blob `oid`; content of blobs not known to be on the
    /// server is collected in `pending`.
    fn blob_id(
        &mut self,
        oid: GitOid,
        pending: &mut BTreeMap<ObjectId, Bytes>,
    ) -> Result<(ObjectId, u64)> {
        if let Some(&(id, size)) = self.blob_ids.get(&oid) {
            if !self.present.contains(&id) && !pending.contains_key(&id) {
                pending.insert(id, Bytes::from(self.git.blob(oid)?));
            }
            return Ok((id, size));
        }
        let content = self.git.blob(oid)?;
        let id = ObjectId::hash_blob(&content);
        let size = content.len() as u64;
        self.blob_ids.insert(oid, (id, size));
        if !self.present.contains(&id) {
            pending.insert(id, Bytes::from(content));
        }
        Ok((id, size))
    }

    /// HasObjects, then UploadObjects for the missing blobs in batches of up to
    /// `batch_bytes` of content (a larger blob travels alone).
    async fn ensure_uploaded(&mut self, mut pending: BTreeMap<ObjectId, Bytes>) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        let ids: Vec<ObjectId> = pending.keys().copied().collect();
        for chunk in ids.chunks(HAS_OBJECTS_CHUNK) {
            let present = self.rpc.has_objects(chunk).await?;
            for (id, present) in chunk.iter().zip(present) {
                if present {
                    pending.remove(id);
                    self.present.insert(*id);
                }
            }
        }
        let limit = self.opts.batch_bytes.max(1);
        let mut batch: Vec<(ObjectId, Bytes)> = Vec::new();
        let mut batch_bytes = 0usize;
        for (id, data) in pending {
            if !batch.is_empty() && batch_bytes.saturating_add(data.len()) > limit {
                self.upload_batch(std::mem::take(&mut batch)).await?;
                batch_bytes = 0;
            }
            batch_bytes = batch_bytes.saturating_add(data.len());
            batch.push((id, data));
        }
        if !batch.is_empty() {
            self.upload_batch(batch).await?;
        }
        Ok(())
    }

    async fn upload_batch(&mut self, batch: Vec<(ObjectId, Bytes)>) -> Result<()> {
        let ids: Vec<ObjectId> = batch.iter().map(|(id, _)| *id).collect();
        let items: Vec<v1::UploadItem> = batch
            .into_iter()
            .map(|(id, data)| v1::UploadItem {
                id: id_bytes(&id),
                kind: u32::from(Kind::Blob.byte()),
                data: data.to_vec(),
            })
            .collect();
        let results = self.rpc.upload(&items).await?;
        for r in &results {
            if r.status != v1::ItemStatus::Ok as i32 {
                let code = ErrorCode::parse(&r.error_code).unwrap_or(ErrorCode::Unavailable);
                let id = parse_id(&r.id, "id")
                    .map(|i| i.to_text())
                    .unwrap_or_default();
                return Err(ScionError::new(
                    code,
                    format!("upload of blob {id} failed: {}", r.message),
                ));
            }
        }
        if results.len() != ids.len() {
            return Err(internal(
                "UploadObjects answered a different number of items",
            ));
        }
        self.report.blobs_uploaded += ids.len() as u64;
        self.present.extend(ids);
        Ok(())
    }

    /// Create or fast-forward the target ref to `tip`; returns the ref version.
    async fn update_target(&mut self, tip: ObjectId, git_tip: &GitOid) -> Result<u64> {
        let name = self.opts.target_ref.as_str();
        let Some((cur, version)) = self.rpc.resolve_ref(name).await? else {
            return self
                .rpc
                .create_ref(name, &tip, &format!("scion-import git:{git_tip}"))
                .await;
        };
        if cur == tip {
            return Ok(version);
        }
        let force = if self.rpc.is_ancestor(&cur, &tip).await? {
            false
        } else if self.rpc.is_ancestor(&tip, &cur).await? {
            // The ref already contains the imported tip (native work landed on top of an
            // earlier import): there is nothing to fast-forward, and rewinding is refused.
            tracing::info!(
                target_ref = name,
                "scion-import: target already contains the imported tip; ref unchanged"
            );
            return Ok(version);
        } else if self.is_bootstrap_root(&cur).await? {
            true
        } else {
            return Err(ScionError::new(
                ErrorCode::NotFastForward,
                format!(
                    "ref {name} at {cur} is not an ancestor of the imported tip {tip}; \
                     refusing to rewind it"
                ),
            ));
        };
        self.rpc.update_ref(name, version, &tip, force).await
    }

    /// True when `id` is the repository's bootstrap root changeset (implementation-defined
    /// 6.5): no parents, the empty root tree and the root change id.
    async fn is_bootstrap_root(&mut self, id: &ObjectId) -> Result<bool> {
        let Some(enc) = self.rpc.changeset_encoding(id).await? else {
            return Ok(false);
        };
        let cs = Changeset::decode(&enc)?;
        Ok(cs.parents.is_empty()
            && cs.root_tree == empty_tree_id()
            && cs.change_id.0 == bootstrap_root_change_id())
    }
}

/// Lowercase hex of `b`.
pub(crate) fn hex_lower(b: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex() {
        assert_eq!(hex_lower(&[0x01, 0xab, 0xff]), "01abff");
    }

    #[test]
    fn root_change_id_is_blake3_prefix() {
        let d = blake3::hash(b"scion-root-v1");
        assert_eq!(&bootstrap_root_change_id()[..], &d.as_bytes()[..16]);
    }

    /// Compile-time check: the import future is `Send` (callers may spawn it).
    #[allow(dead_code)]
    fn import_future_is_send(c: &ScionClient, o: &ImportOptions) {
        fn assert_send<T: Send>(_: T) {}
        assert_send(import(c, o));
    }
}
