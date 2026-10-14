//! Import fidelity helpers for P30/P33 (feature `testing`; SPEC 19.14; P29).
//!
//! - [`importer_acl`], [`IMPORTER_SERVICE`], [`IMPORTER_SECRET`]: a policy and service
//!   principal with write on `/`, `admin.refs` and `force` on every ref, as the importer needs
//!   (implementation-defined 6.4).
//! - [`git_commits`] / [`git_files`]: the git side read with gix. Signatures are parsed by
//!   gix here (not by the importer's own parser), so the comparison is independent.
//! - [`ScionReader`]: the Scion side read over RPC (`GetChangesets`, `GetObjects` range reads
//!   and tree walks), with per-id caches.
//! - [`check_fidelity`]: the SPEC 19.14 differential for every commit reachable from a git
//!   ref: tree file sets, modes and contents, Meta fields byte for byte, extension tags 2 and
//!   4, parents, change ids, and `git_mapping` in both directions.

use crate::import::{GitOid, GitRepo, hex_lower, oid_bytes};
use crate::mapping::{
    GIT_MODE_GITLINK, GIT_MODE_TREE, change_id_for, decode_skipped_gitlinks, map_mode,
};
use crate::{ImportOptions, ImportReport};
use bytes::{Bytes, BytesMut};
use scion_core::{ErrorCode, Kind, Mode, ObjectId, Principal, Result, ScionError};
use scion_object::{
    Canonical, Changeset, META_EXT_GIT_COMMIT, META_EXT_SKIPPED_GITLINKS, Meta, RefName, Timestamp,
    Tree,
};
use scion_proto::convert::{id_bytes, parse_id};
use scion_proto::{ScionClient, error_from_status, v1};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// Service principal name used by import tests (`service:importer`).
pub const IMPORTER_SERVICE: &str = "importer";
/// Plaintext secret of [`IMPORTER_SERVICE`] in tests.
pub const IMPORTER_SECRET: &str = "importer-test-secret";
/// Default upload batch size (bytes of content per UploadObjects call).
pub const DEFAULT_BATCH_BYTES: usize = 8 * 1024 * 1024;

/// `service:importer` (implementation-defined 6.4).
pub fn importer_principal() -> Principal {
    Principal::Service(IMPORTER_SERVICE.to_owned())
}

/// An `acl.toml` where group `eng` (the default test humans plus `service:importer`) reads and
/// writes `/`, and `service:importer` additionally holds `admin.refs` and `force` on every
/// ref (implementation-defined 6.4; importing into a ref still at the bootstrap root is a
/// forced update). `github:admin` holds every admin permission.
pub fn importer_acl() -> String {
    r#"version = 1

[groups]
eng = ["github:alice", "github:bob", "github:carol", "service:ci", "service:agent", "service:importer", "group:admins"]
admins = ["github:admin"]
importers = ["service:importer"]

[[paths]]
prefix = "/"
read = ["group:eng"]
write = ["group:eng"]

[refs."*"]
update = ["group:eng"]
force = ["group:admins", "group:importers"]

[admin]
redact = ["group:admins"]
purge = ["group:admins"]
refs = ["group:admins", "group:importers"]
"#
    .to_owned()
}

/// Import options with [`DEFAULT_BATCH_BYTES`] (panics on an invalid ref name: test helper).
pub fn options(
    git_dir: &Path,
    git_ref: &str,
    target_ref: &str,
    skip_submodules: bool,
) -> ImportOptions {
    ImportOptions {
        git_dir: git_dir.to_path_buf(),
        git_ref: git_ref.to_owned(),
        target_ref: RefName::parse(target_ref).unwrap_or_else(|_| RefName::main()),
        skip_submodules,
        batch_bytes: DEFAULT_BATCH_BYTES,
    }
}

/// Run [`crate::import`] (convenience for scenario code).
pub async fn run_import(client: &ScionClient, opts: &ImportOptions) -> Result<ImportReport> {
    crate::import(client, opts).await
}

fn mismatch(msg: impl Into<String>) -> ScionError {
    ScionError::new(ErrorCode::Integrity, msg)
}

fn status_err(s: tonic::Status) -> ScionError {
    error_from_status(&s)
}

/// One git commit as read with gix (SPEC 19.14 comparison side).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitCommitView {
    /// 40-hex commit id.
    pub id: String,
    /// 40-hex parent ids, in order.
    pub parents: Vec<String>,
    /// 40-hex root tree id.
    pub tree: String,
    /// `Name <email>` as parsed by gix.
    pub author: Vec<u8>,
    /// Author time and zone (minutes).
    pub author_time: Timestamp,
    /// `Name <email>` as parsed by gix.
    pub committer: Vec<u8>,
    /// Committer time and zone (minutes).
    pub committer_time: Timestamp,
    /// The message as parsed by gix.
    pub message: Vec<u8>,
}

fn gix_err(what: &str, e: impl std::fmt::Display) -> ScionError {
    ScionError::new(ErrorCode::InvalidArgument, format!("{what}: {e}"))
}

fn ident(sig: &gix::actor::SignatureRef<'_>) -> Vec<u8> {
    let mut v = sig.name.to_vec();
    v.extend_from_slice(b" <");
    v.extend_from_slice(sig.email);
    v.push(b'>');
    v
}

fn gix_time(sig: &gix::actor::SignatureRef<'_>) -> Result<Timestamp> {
    let t = sig.time().map_err(|e| gix_err("signature time", e))?;
    Ok(Timestamp {
        seconds: t.seconds,
        tz_offset_minutes: i64::from(t.offset) / 60,
    })
}

/// Every commit reachable from `git_ref`, parents first (SPEC 19.14).
pub fn git_commits(git_dir: &Path, git_ref: &str) -> Result<Vec<GitCommitView>> {
    let repo = GitRepo::open(git_dir)?;
    let tip = repo.resolve_commit(git_ref)?;
    let mut out = Vec::new();
    for c in repo.topo_order(tip)? {
        let cr = gix::objs::CommitRef::from_bytes(&c.raw, gix::hash::Kind::Sha1)
            .map_err(|e| gix_err("commit", e))?;
        let a = cr.author().map_err(|e| gix_err("author", e))?;
        let m = cr.committer().map_err(|e| gix_err("committer", e))?;
        out.push(GitCommitView {
            id: c.id.to_string(),
            parents: c.parents.iter().map(|p| p.to_string()).collect(),
            tree: c.tree.to_string(),
            author: ident(&a),
            author_time: gix_time(&a)?,
            committer: ident(&m),
            committer_time: gix_time(&m)?,
            message: cr.message.to_vec(),
        });
    }
    Ok(out)
}

/// One file of a git tree, mapped to Scion terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedFile {
    /// The Scion mode the git mode maps to.
    pub mode: Mode,
    /// The git mode as written in the tree.
    pub git_mode: u32,
    /// 40-hex git blob id.
    pub git_oid: String,
    /// Blob content.
    pub content: Bytes,
}

/// A git tree's files and gitlinks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitTreeFiles {
    /// File-level entries by path (gitlinks and empty trees excluded: SPEC 14 exclusions).
    pub files: BTreeMap<Vec<u8>, ExpectedFile>,
    /// Gitlinks as `(path, 20-byte sha)`, ascending by path.
    pub gitlinks: Vec<(Vec<u8>, [u8; 20])>,
}

/// The files of commit `commit_hex`'s tree read with gix (SPEC 19.14).
pub fn git_files(git_dir: &Path, commit_hex: &str) -> Result<GitTreeFiles> {
    let repo = GitRepo::open(git_dir)?;
    let mut cache = HashMap::new();
    git_files_with(&repo, commit_hex, &mut cache)
}

fn git_files_with(
    repo: &GitRepo,
    commit_hex: &str,
    blobs: &mut HashMap<GitOid, Bytes>,
) -> Result<GitTreeFiles> {
    let id = repo.resolve_commit(commit_hex)?;
    let c = repo.commit(id)?;
    let mut out = GitTreeFiles::default();
    let mut stack: Vec<(Vec<u8>, GitOid)> = vec![(Vec::new(), c.tree)];
    while let Some((prefix, t)) = stack.pop() {
        for e in repo.tree_entries(t)? {
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(&e.name);
            if e.mode == GIT_MODE_TREE {
                stack.push((path, e.oid));
            } else if e.mode == GIT_MODE_GITLINK {
                out.gitlinks.push((path, oid_bytes(&e.oid)?));
            } else {
                let mode =
                    map_mode(e.mode)?.ok_or_else(|| mismatch("tree mode on a non-tree entry"))?;
                let content = match blobs.get(&e.oid) {
                    Some(b) => b.clone(),
                    None => {
                        let b = Bytes::from(repo.blob(e.oid)?);
                        blobs.insert(e.oid, b.clone());
                        b
                    }
                };
                out.files.insert(
                    path,
                    ExpectedFile {
                        mode,
                        git_mode: e.mode,
                        git_oid: e.oid.to_string(),
                        content,
                    },
                );
            }
        }
    }
    out.gitlinks.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// One file of a Scion tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScionFile {
    /// Mode (1..=3).
    pub mode: Mode,
    /// Blob id.
    pub id: ObjectId,
    /// Blob size.
    pub size: u64,
}

/// `Resolve("git:<sha>")`: the imported changeset, `None` when not imported (SPEC 14).
pub async fn resolve_git(client: &ScionClient, sha_hex: &str) -> Result<Option<ObjectId>> {
    match client
        .query()
        .resolve(v1::ResolveRequest {
            revision: format!("git:{sha_hex}"),
        })
        .await
    {
        Ok(r) => Ok(Some(parse_id(&r.into_inner().changeset, "changeset")?)),
        Err(s) => {
            let e = status_err(s);
            if e.code == ErrorCode::NotFound {
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

/// `GetDerived(git_mapping, "c:" || changeset id)`: the 40-hex git commit a changeset was
/// imported from, `None` when it has no mapping (SPEC 9.2; implementation-defined 6.4).
pub async fn git_for_changeset(client: &ScionClient, cs: &ObjectId) -> Result<Option<String>> {
    let mut key = b"c:".to_vec();
    key.extend_from_slice(cs.as_bytes());
    match client
        .query()
        .get_derived(v1::GetDerivedRequest {
            r#type: "git_mapping".to_owned(),
            version: None,
            key,
        })
        .await
    {
        Ok(r) => Ok(Some(
            String::from_utf8_lossy(&r.into_inner().value).into_owned(),
        )),
        Err(s) => {
            let e = status_err(s);
            if e.code == ErrorCode::NotFound {
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

/// Reads the Scion side over RPC with per-id caches (immutable objects).
pub struct ScionReader<'a> {
    client: &'a ScionClient,
    trees: HashMap<ObjectId, Tree>,
    blobs: HashMap<ObjectId, Bytes>,
}

impl<'a> ScionReader<'a> {
    /// A reader over `client`.
    pub fn new(client: &'a ScionClient) -> Self {
        ScionReader {
            client,
            trees: HashMap::new(),
            blobs: HashMap::new(),
        }
    }

    /// Raw (decompressed) content of objects via `GetObjects` range reads (SPEC 7.3),
    /// digest-checked. Any item that is not `OK` is an error.
    pub async fn raw_objects(&self, ids: &[ObjectId]) -> Result<HashMap<ObjectId, Bytes>> {
        let mut out: HashMap<ObjectId, BytesMut> = HashMap::new();
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut stream = self
            .client
            .objects()
            .get_objects(v1::GetObjectsRequest {
                ids: ids.iter().map(id_bytes).collect(),
                offset: Some(0),
                length: None,
            })
            .await
            .map_err(status_err)?
            .into_inner();
        while let Some(chunk) = stream.message().await.map_err(status_err)? {
            let id = parse_id(&chunk.id, "id")?;
            if chunk.status != v1::ItemStatus::Ok as i32 {
                return Err(ScionError::new(
                    ErrorCode::NotFound,
                    format!("GetObjects {id}: status {}", chunk.status),
                ));
            }
            if chunk.encoding != v1::Encoding::Raw as i32 {
                return Err(mismatch(format!("GetObjects {id}: range read was not RAW")));
            }
            let buf = out.entry(id).or_default();
            if chunk.offset != buf.len() as u64 {
                return Err(mismatch(format!("GetObjects {id}: chunk offset gap")));
            }
            buf.extend_from_slice(&chunk.data);
        }
        let mut done = HashMap::new();
        for (id, b) in out {
            let b = b.freeze();
            let got = match id.kind() {
                Kind::Blob => ObjectId::hash_blob(&b),
                k => ObjectId::hash_encoded(k, &b),
            };
            if got != id {
                return Err(ScionError::integrity(&id));
            }
            done.insert(id, b);
        }
        for id in ids {
            if !done.contains_key(id) {
                return Err(ScionError::not_found(format!(
                    "GetObjects returned no {id}"
                )));
            }
        }
        Ok(done)
    }

    /// A changeset (canonical encoding from `GetChangesets`).
    pub async fn changeset(&self, id: &ObjectId) -> Result<Changeset> {
        let resp = self
            .client
            .query()
            .get_changesets(v1::GetChangesetsRequest {
                ids: vec![id_bytes(id)],
            })
            .await
            .map_err(status_err)?
            .into_inner();
        let item = resp
            .items
            .first()
            .ok_or_else(|| ScionError::not_found(format!("changeset {id}")))?;
        match &item.view {
            Some(v1::changeset_item::View::Encoded(enc)) => Changeset::decode(enc),
            _ => Err(ScionError::not_found(format!(
                "changeset {id} (status {})",
                item.status
            ))),
        }
    }

    /// A Meta object.
    pub async fn meta(&self, id: &ObjectId) -> Result<Meta> {
        let m = self.raw_objects(&[*id]).await?;
        let b = m
            .get(id)
            .ok_or_else(|| ScionError::not_found(format!("meta {id}")))?;
        Meta::decode(b)
    }

    /// Blob content (cached).
    pub async fn blob(&mut self, id: &ObjectId) -> Result<Bytes> {
        if let Some(b) = self.blobs.get(id) {
            return Ok(b.clone());
        }
        let m = self.raw_objects(&[*id]).await?;
        let b = m
            .get(id)
            .cloned()
            .ok_or_else(|| ScionError::not_found(format!("blob {id}")))?;
        self.blobs.insert(*id, b.clone());
        Ok(b)
    }

    /// Every file under `root_tree` (a tree walk over `GetObjects`; trees cached by id).
    pub async fn files(&mut self, root_tree: &ObjectId) -> Result<BTreeMap<Vec<u8>, ScionFile>> {
        let mut out = BTreeMap::new();
        let mut level: Vec<(Vec<u8>, ObjectId)> = vec![(Vec::new(), *root_tree)];
        while !level.is_empty() {
            let missing: Vec<ObjectId> = {
                let mut v: Vec<ObjectId> = level
                    .iter()
                    .map(|(_, t)| *t)
                    .filter(|t| !self.trees.contains_key(t))
                    .collect();
                v.sort();
                v.dedup();
                v
            };
            for (id, raw) in self.raw_objects(&missing).await? {
                self.trees.insert(id, Tree::decode(&raw)?);
            }
            let mut next = Vec::new();
            for (prefix, t) in level {
                let tree = self
                    .trees
                    .get(&t)
                    .ok_or_else(|| ScionError::not_found(format!("tree {t}")))?;
                for e in &tree.entries {
                    let mut path = prefix.clone();
                    if !path.is_empty() {
                        path.push(b'/');
                    }
                    path.extend_from_slice(e.name.as_bytes());
                    if e.mode == Mode::Tree {
                        next.push((path, e.id));
                    } else {
                        out.insert(
                            path,
                            ScionFile {
                                mode: e.mode,
                                id: e.id,
                                size: e.size,
                            },
                        );
                    }
                }
            }
            level = next;
        }
        Ok(out)
    }
}

/// Result of [`check_fidelity`]. `mismatches` is empty when the import is faithful.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FidelityReport {
    /// Commits compared.
    pub commits_checked: usize,
    /// File entries compared (summed over commits).
    pub files_checked: usize,
    /// Distinct blobs whose bytes were fetched and compared.
    pub blobs_compared: usize,
    /// Skipped gitlinks seen (summed over commits).
    pub gitlinks_excluded: usize,
    /// Human-readable differences.
    pub mismatches: Vec<String>,
}

/// SPEC 19.14 differential for every commit reachable from `git_ref`: the imported changeset
/// exists (`Resolve("git:<sha>")`) and maps back (`git_mapping` `c:` key); its parents are
/// the mapped git parents and its change id is [`change_id_for`]; its tree's file set, modes
/// and contents equal the git tree's (gitlinks and empty trees excluded); its Meta preserves
/// author, committer, both timestamps and zones and the message byte for byte, with extension
/// tag 2 = the commit id and tag 4 = the tree's gitlinks (absent when there are none).
pub async fn check_fidelity(
    client: &ScionClient,
    git_dir: &Path,
    git_ref: &str,
) -> Result<FidelityReport> {
    let repo = GitRepo::open(git_dir)?;
    let commits = git_commits(git_dir, git_ref)?;
    let mut reader = ScionReader::new(client);
    let mut report = FidelityReport::default();
    let mut mapped: HashMap<String, ObjectId> = HashMap::new();
    let mut git_blobs: HashMap<GitOid, Bytes> = HashMap::new();
    let mut verified: HashMap<ObjectId, ()> = HashMap::new();
    for c in &commits {
        report.commits_checked += 1;
        let mut errs: Vec<String> = Vec::new();
        let Some(cs_id) = resolve_git(client, &c.id).await? else {
            report
                .mismatches
                .push(format!("commit {}: Resolve(git:<sha>) is NOT_FOUND", c.id));
            continue;
        };
        mapped.insert(c.id.clone(), cs_id);
        match git_for_changeset(client, &cs_id).await? {
            Some(back) if back == c.id => {}
            other => errs.push(format!("git_mapping c: key gives {other:?}")),
        }
        let cs = reader.changeset(&cs_id).await?;
        let want_parents: Vec<Option<ObjectId>> =
            c.parents.iter().map(|p| mapped.get(p).copied()).collect();
        let got_parents: Vec<Option<ObjectId>> = cs.parents.iter().map(|p| Some(*p)).collect();
        if want_parents != got_parents {
            errs.push(format!(
                "parents {got_parents:?} != mapped git parents {want_parents:?}"
            ));
        }
        let sha =
            oid_bytes(&GitOid::from_hex(c.id.as_bytes()).map_err(|e| gix_err("commit id", e))?)?;
        if cs.change_id != change_id_for(&sha) {
            errs.push("change id differs from BLAKE3(scion-import-v1 || sha)".into());
        }

        // Meta.
        let meta = reader.meta(&cs.meta).await?;
        if meta.author != c.author {
            errs.push(format!(
                "author {:?} != {:?}",
                String::from_utf8_lossy(&meta.author),
                String::from_utf8_lossy(&c.author)
            ));
        }
        if meta.author_time != c.author_time {
            errs.push(format!(
                "author time {:?} != {:?}",
                meta.author_time, c.author_time
            ));
        }
        if meta.committer.as_deref() != Some(c.committer.as_slice()) {
            errs.push("committer differs".into());
        }
        if meta.committer_time != Some(c.committer_time) {
            errs.push(format!(
                "committer time {:?} != {:?}",
                meta.committer_time, c.committer_time
            ));
        }
        if meta.message != c.message {
            errs.push("message differs".into());
        }
        if meta.extensions.get(META_EXT_GIT_COMMIT) != Some(&sha[..]) {
            errs.push("extension tag 2 is not the git commit id".into());
        }

        // Tree.
        let expected = git_files_with(&repo, &c.id, &mut git_blobs)?;
        report.gitlinks_excluded += expected.gitlinks.len();
        match meta.extensions.get(META_EXT_SKIPPED_GITLINKS) {
            None if expected.gitlinks.is_empty() => {}
            None => errs.push("gitlinks present but extension tag 4 is absent".into()),
            Some(v) => match decode_skipped_gitlinks(v) {
                Ok(links) if links == expected.gitlinks => {}
                Ok(links) => errs.push(format!(
                    "extension tag 4 lists {} gitlinks, tree has {}",
                    links.len(),
                    expected.gitlinks.len()
                )),
                Err(e) => errs.push(format!("extension tag 4 undecodable: {e}")),
            },
        }
        let got = reader.files(&cs.root_tree).await?;
        let got_paths: Vec<&Vec<u8>> = got.keys().collect();
        let want_paths: Vec<&Vec<u8>> = expected.files.keys().collect();
        if got_paths != want_paths {
            errs.push(format!(
                "file sets differ: scion {} paths, git {} paths",
                got_paths.len(),
                want_paths.len()
            ));
        }
        for (path, want) in &expected.files {
            let Some(have) = got.get(path) else { continue };
            report.files_checked += 1;
            let p = String::from_utf8_lossy(path).into_owned();
            if have.mode != want.mode {
                errs.push(format!("{p}: mode {:?} != {:?}", have.mode, want.mode));
            }
            if have.size != want.content.len() as u64 {
                errs.push(format!("{p}: size {} != {}", have.size, want.content.len()));
            }
            if have.id != ObjectId::hash_blob(&want.content) {
                errs.push(format!("{p}: blob id differs from the git content's"));
                continue;
            }
            if verified.insert(have.id, ()).is_none() {
                report.blobs_compared += 1;
                let bytes = reader.blob(&have.id).await?;
                if bytes != want.content {
                    errs.push(format!("{p}: content differs"));
                }
            }
        }
        report
            .mismatches
            .extend(errs.into_iter().map(|m| format!("commit {}: {m}", c.id)));
    }
    Ok(report)
}

/// Lowercase hex of a 20-byte git sha (helper for gitlink comparisons).
pub fn sha_hex(sha: &[u8; 20]) -> String {
    hex_lower(sha)
}
