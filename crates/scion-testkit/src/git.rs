//! Vendored git histories and git fixtures (SPEC 19.0, 19.14; P09).
//!
//! - [`bundles`] lists the vendored bundles under `corpus/git/` (two real open-source
//!   histories plus a synthetic edge history; see `corpus/git/README.md`).
//! - [`unpack`] turns a bundle into a bare repository with gix only (no git CLI): the bundle
//!   header is parsed here, the pack is indexed by `gix-pack`, and refs are written as
//!   `packed-refs`.
//! - [`commits`] and [`ls_tree`] read a repository with gix: the comparison side of the
//!   SPEC 19.14 differential when the git CLI is absent.
//! - [`GitFixture`] builds small repositories from a [`GitOp`] script with the git CLI
//!   (plumbing only, deterministic dates). Callers skip when [`git_available`] is false.

use scion_core::{ErrorCode, Result, ScionError};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;

/// Maximum size of one vendored bundle (PLAN P09: each at most 5 MB).
pub const MAX_BUNDLE_BYTES: u64 = 5 * 1024 * 1024;

fn unavailable(msg: impl Into<String>) -> ScionError {
    ScionError::new(ErrorCode::Unavailable, msg)
}

fn invalid(msg: impl Into<String>) -> ScionError {
    ScionError::new(ErrorCode::InvalidArgument, msg)
}

/// The `corpus/git/` directory of the workspace (overridable with `SCION_CORPUS_GIT_DIR`).
pub fn corpus_dir() -> PathBuf {
    if let Ok(d) = std::env::var("SCION_CORPUS_GIT_DIR")
        && !d.is_empty()
    {
        return PathBuf::from(d);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("corpus")
        .join("git")
}

/// Every vendored bundle under `corpus/git/` (SPEC 19.0 Corpus), sorted by file name.
pub fn bundles() -> Vec<PathBuf> {
    let dir = corpus_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        tracing::warn!(dir = %dir.display(), "corpus/git directory is missing");
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "bundle"))
        .collect();
    out.sort();
    out
}

/// A parsed bundle header (git bundle format v2 or v3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleHeader {
    /// Format version (2 or 3).
    pub version: u8,
    /// `(ref name, 40-hex object id)` in header order; may include `HEAD`.
    pub refs: Vec<(String, String)>,
    /// Prerequisite object ids (thin bundles); vendored bundles have none.
    pub prerequisites: Vec<String>,
}

/// Parse the header of `bundle` (SPEC 19.14 corpus; no git CLI).
pub fn bundle_header(bundle: &Path) -> Result<BundleHeader> {
    let f = std::fs::File::open(bundle)
        .map_err(|e| unavailable(format!("open {}: {e}", bundle.display())))?;
    let mut r = BufReader::new(f);
    read_header(&mut r)
}

fn read_header(r: &mut impl BufRead) -> Result<BundleHeader> {
    let mut line = Vec::new();
    let mut next_line = |r: &mut dyn BufRead| -> Result<Option<String>> {
        line.clear();
        let n = r
            .read_until(b'\n', &mut line)
            .map_err(|e| unavailable(format!("read bundle: {e}")))?;
        if n == 0 {
            return Ok(None);
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        String::from_utf8(line.clone())
            .map(Some)
            .map_err(|_| invalid("bundle header is not UTF-8"))
    };
    let sig = next_line(r)?.unwrap_or_default();
    let version = match sig.as_str() {
        "# v2 git bundle" => 2,
        "# v3 git bundle" => 3,
        other => return Err(invalid(format!("not a git bundle: {other:?}"))),
    };
    let mut header = BundleHeader {
        version,
        refs: Vec::new(),
        prerequisites: Vec::new(),
    };
    loop {
        let Some(l) = next_line(r)? else {
            return Err(invalid("bundle header is not terminated"));
        };
        if l.is_empty() {
            break;
        }
        if let Some(cap) = l.strip_prefix('@') {
            if version == 3 && (cap == "object-format=sha1" || cap.starts_with("filter=")) {
                if cap.starts_with("filter=") {
                    return Err(invalid("filtered bundles are not supported"));
                }
                continue;
            }
            return Err(invalid(format!("unsupported bundle capability {cap:?}")));
        }
        if let Some(pre) = l.strip_prefix('-') {
            let id = pre.split(' ').next().unwrap_or_default().to_string();
            header.prerequisites.push(id);
            continue;
        }
        let Some((id, name)) = l.split_once(' ') else {
            return Err(invalid(format!("bad bundle ref line {l:?}")));
        };
        if id.len() != 40 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid(format!("bad object id in bundle ref line {l:?}")));
        }
        header
            .refs
            .push((name.to_string(), id.to_ascii_lowercase()));
    }
    Ok(header)
}

/// Unpack `bundle` into a bare repository under `dst` (via gix; no git CLI); returns its path.
///
/// The repository is created at `dst/<bundle stem>.git`. All bundle refs are written to
/// `packed-refs`; `HEAD` points at the branch the bundle's `HEAD` names (preferring `main`,
/// then `master`), or at the first branch. Thin bundles (with prerequisites) are rejected.
/// SPEC 19.14 (corpus for import differentials).
pub fn unpack(bundle: &Path, dst: &Path) -> Result<PathBuf> {
    let stem = bundle
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid(format!("bad bundle file name {}", bundle.display())))?;
    let f = std::fs::File::open(bundle)
        .map_err(|e| unavailable(format!("open {}: {e}", bundle.display())))?;
    let mut reader = BufReader::with_capacity(1 << 16, f);
    let header = read_header(&mut reader)?;
    if !header.prerequisites.is_empty() {
        return Err(invalid(
            "thin bundles (with prerequisites) are not supported",
        ));
    }

    let repo_dir = dst.join(format!("{stem}.git"));
    std::fs::create_dir_all(dst)
        .map_err(|e| unavailable(format!("create {}: {e}", dst.display())))?;
    gix::init_bare(&repo_dir)
        .map_err(|e| unavailable(format!("init {}: {e}", repo_dir.display())))?;
    let pack_dir = repo_dir.join("objects").join("pack");
    std::fs::create_dir_all(&pack_dir)
        .map_err(|e| unavailable(format!("create {}: {e}", pack_dir.display())))?;

    let interrupt = AtomicBool::new(false);
    gix::odb::pack::Bundle::write_to_directory(
        &mut reader,
        Some(&pack_dir),
        &mut gix::progress::Discard,
        &interrupt,
        None::<gix::objs::find::Never>,
        gix::hash::Kind::Sha1,
        gix::odb::pack::bundle::write::Options::default(),
    )
    .map_err(|e| invalid(format!("index pack of {}: {e}", bundle.display())))?;

    // Refs: packed-refs, sorted by name (HEAD is handled separately).
    let mut refs: BTreeMap<&str, &str> = BTreeMap::new();
    let mut head_id: Option<&str> = None;
    for (name, id) in &header.refs {
        if name == "HEAD" {
            head_id = Some(id);
        } else if name.starts_with("refs/") {
            refs.insert(name, id);
        }
    }
    let mut packed = String::from("# pack-refs with: sorted \n");
    for (name, id) in &refs {
        packed.push_str(&format!("{id} {name}\n"));
    }
    write_file(&repo_dir.join("packed-refs"), packed.as_bytes())?;

    let branches: Vec<&str> = refs
        .keys()
        .copied()
        .filter(|n| n.starts_with("refs/heads/"))
        .collect();
    let preferred = ["refs/heads/main", "refs/heads/master"];
    let head = head_id
        .and_then(|hid| {
            preferred
                .iter()
                .copied()
                .find(|p| refs.get(p) == Some(&hid))
                .or_else(|| branches.iter().copied().find(|b| refs.get(b) == Some(&hid)))
        })
        .or_else(|| preferred.iter().copied().find(|p| refs.contains_key(p)))
        .or_else(|| branches.first().copied());
    if let Some(head) = head {
        write_file(&repo_dir.join("HEAD"), format!("ref: {head}\n").as_bytes())?;
    }
    Ok(repo_dir)
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).map_err(|e| unavailable(format!("write {}: {e}", path.display())))
}

/// One commit read with gix (SPEC 19.14 comparison side).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitCommitInfo {
    /// 40-hex commit id.
    pub id: String,
    /// 40-hex parent ids, in order.
    pub parents: Vec<String>,
    /// 40-hex root tree id.
    pub tree: String,
    /// The raw commit object (headers and message, byte for byte).
    pub raw: Vec<u8>,
}

/// One file-level entry of a commit's tree (like `git ls-tree -r`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitEntry {
    /// Full path, `/`-separated bytes.
    pub path: Vec<u8>,
    /// Git mode as written in the tree (e.g. `0o100644`, `0o100664`, `0o120000`, `0o160000`).
    pub mode: u32,
    /// 40-hex object id (blob, or commit for gitlinks).
    pub oid: String,
    /// Blob content; `None` for gitlinks.
    pub content: Option<Vec<u8>>,
}

fn open_repo(repo: &Path) -> Result<gix::Repository> {
    gix::open(repo).map_err(|e| unavailable(format!("open {}: {e}", repo.display())))
}

fn parse_oid(hex: &str) -> Result<gix::ObjectId> {
    gix::ObjectId::from_hex(hex.as_bytes())
        .map_err(|e| invalid(format!("bad object id {hex:?}: {e}")))
}

fn read_object(repo: &gix::Repository, id: gix::ObjectId) -> Result<(gix::objs::Kind, Vec<u8>)> {
    let obj = repo
        .find_object(id)
        .map_err(|e| invalid(format!("missing object {id}: {e}")))?;
    Ok((obj.kind, obj.data.clone()))
}

/// Peel `id` through annotated tags; `None` when it does not end at a commit.
fn peel_to_commit(repo: &gix::Repository, mut id: gix::ObjectId) -> Result<Option<gix::ObjectId>> {
    for _ in 0..16 {
        let (kind, data) = read_object(repo, id)?;
        match kind {
            gix::objs::Kind::Commit => return Ok(Some(id)),
            gix::objs::Kind::Tag => {
                let tag = gix::objs::TagRef::from_bytes(&data, gix::hash::Kind::Sha1)
                    .map_err(|e| invalid(format!("bad tag {id}: {e}")))?;
                id = tag.target();
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// Every commit reachable from any ref of `repo`, parents before children (SPEC 19.14).
/// Deterministic: refs are visited in name order and parents in order.
pub fn commits(repo: &Path) -> Result<Vec<GitCommitInfo>> {
    let r = open_repo(repo)?;
    let mut tips: BTreeMap<String, gix::ObjectId> = BTreeMap::new();
    let platform = r
        .references()
        .map_err(|e| unavailable(format!("refs of {}: {e}", repo.display())))?;
    let iter = platform
        .all()
        .map_err(|e| unavailable(format!("refs of {}: {e}", repo.display())))?;
    for reference in iter {
        let reference = reference.map_err(|e| unavailable(format!("ref: {e}")))?;
        let name = reference.name().as_bstr().to_string();
        if let Some(id) = reference.target().try_id() {
            tips.insert(name, id.to_owned());
        }
    }
    let mut out = Vec::new();
    let mut done: BTreeSet<gix::ObjectId> = BTreeSet::new();
    for tip in tips.values() {
        let Some(tip) = peel_to_commit(&r, *tip)? else {
            continue;
        };
        // Iterative post-order DFS: emit a commit after all its parents.
        let mut stack: Vec<(gix::ObjectId, bool)> = vec![(tip, false)];
        while let Some((id, expanded)) = stack.pop() {
            if done.contains(&id) {
                continue;
            }
            let (_, data) = read_object(&r, id)?;
            let c = gix::objs::CommitRef::from_bytes(&data, gix::hash::Kind::Sha1)
                .map_err(|e| invalid(format!("bad commit {id}: {e}")))?;
            let parents: Vec<gix::ObjectId> = c.parents().collect();
            if expanded {
                done.insert(id);
                out.push(GitCommitInfo {
                    id: id.to_string(),
                    parents: parents.iter().map(|p| p.to_string()).collect(),
                    tree: c.tree().to_string(),
                    raw: data.clone(),
                });
            } else {
                stack.push((id, true));
                for p in parents.iter().rev() {
                    if !done.contains(p) {
                        stack.push((*p, false));
                    }
                }
            }
        }
    }
    Ok(out)
}

/// The file-level entries of `commit`'s tree (like `git ls-tree -r` plus blob contents),
/// ascending by path bytes (SPEC 19.14 differential fidelity).
pub fn ls_tree(repo: &Path, commit: &str) -> Result<Vec<GitEntry>> {
    let r = open_repo(repo)?;
    let id = parse_oid(commit)?;
    let id = peel_to_commit(&r, id)?.ok_or_else(|| invalid(format!("{commit} is not a commit")))?;
    let (_, data) = read_object(&r, id)?;
    let c = gix::objs::CommitRef::from_bytes(&data, gix::hash::Kind::Sha1)
        .map_err(|e| invalid(format!("bad commit {id}: {e}")))?;
    let mut out = Vec::new();
    let mut stack: Vec<(Vec<u8>, gix::ObjectId)> = vec![(Vec::new(), c.tree())];
    while let Some((prefix, tree_id)) = stack.pop() {
        let (kind, data) = read_object(&r, tree_id)?;
        if kind != gix::objs::Kind::Tree {
            return Err(invalid(format!("{tree_id} is not a tree")));
        }
        let tree = gix::objs::TreeRef::from_bytes(&data, gix::hash::Kind::Sha1)
            .map_err(|e| invalid(format!("bad tree {tree_id}: {e}")))?;
        for e in tree.entries {
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(e.filename);
            let oid = e.oid.to_owned();
            let mode = u32::from(e.mode.value());
            match e.mode.kind() {
                gix::objs::tree::EntryKind::Tree => stack.push((path, oid)),
                gix::objs::tree::EntryKind::Commit => out.push(GitEntry {
                    path,
                    mode,
                    oid: oid.to_string(),
                    content: None,
                }),
                _ => {
                    let (_, blob) = read_object(&r, oid)?;
                    out.push(GitEntry {
                        path,
                        mode,
                        oid: oid.to_string(),
                        content: Some(blob),
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// git CLI fixtures
// ---------------------------------------------------------------------------------------------

/// True when the `git` CLI is available (fixture builders and differential tests).
pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A temporary git repository built by the `git` CLI from a script.
pub struct GitFixture {
    /// The repository directory.
    pub dir: tempfile::TempDir,
}

/// A tree entry while building fixture trees: `(git mode, 40-hex id)`.
type FixtureEntry = (u32, String);

struct Runner<'a> {
    dir: &'a Path,
    clock: i64,
}

impl Runner<'_> {
    fn git(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut cmd = Command::new("git");
        cmd.current_dir(self.dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("HOME", self.dir)
            .env("GIT_AUTHOR_NAME", "Scion Fixture Author")
            .env("GIT_AUTHOR_EMAIL", "author@example.com")
            .env("GIT_COMMITTER_NAME", "Scion Fixture Committer")
            .env("GIT_COMMITTER_EMAIL", "committer@example.com")
            .env("GIT_AUTHOR_DATE", format!("@{} +0000", self.clock))
            .env("GIT_COMMITTER_DATE", format!("@{} +0000", self.clock))
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| unavailable(format!("spawn git {args:?}: {e}")))?;
        if let Some(input) = stdin
            && let Some(mut si) = child.stdin.take()
        {
            si.write_all(input)
                .map_err(|e| unavailable(format!("git {args:?} stdin: {e}")))?;
        }
        let out = child
            .wait_with_output()
            .map_err(|e| unavailable(format!("git {args:?}: {e}")))?;
        if !out.status.success() {
            return Err(unavailable(format!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(out.stdout)
    }

    fn git_line(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<String> {
        let out = self.git(args, stdin)?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    fn tip(&self, branch: &str) -> Result<Option<String>> {
        let refname = format!("refs/heads/{branch}");
        match self.git(&["rev-parse", "--verify", "-q", &refname], None) {
            Ok(out) => Ok(Some(String::from_utf8_lossy(&out).trim().to_string())),
            Err(_) => Ok(None),
        }
    }

    /// Flat file-level entries of a commit (`ls-tree -r`, gitlinks included).
    fn entries(&self, commit: Option<&str>) -> Result<BTreeMap<Vec<u8>, FixtureEntry>> {
        let mut out = BTreeMap::new();
        let Some(commit) = commit else {
            return Ok(out);
        };
        let raw = self.git(&["ls-tree", "-r", "-z", "--full-tree", commit], None)?;
        for rec in raw.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let Some(tab) = rec.iter().position(|b| *b == b'\t') else {
                return Err(invalid("unexpected ls-tree output"));
            };
            let meta = String::from_utf8_lossy(&rec[..tab]).to_string();
            let mut parts = meta.split(' ');
            let mode = parts
                .next()
                .and_then(|m| u32::from_str_radix(m, 8).ok())
                .ok_or_else(|| invalid("unexpected ls-tree mode"))?;
            let _kind = parts.next();
            let oid = parts.next().unwrap_or_default().to_string();
            out.insert(rec[tab + 1..].to_vec(), (mode, oid));
        }
        Ok(out)
    }

    /// Build trees bottom-up with `git mktree` (preserves any mode, e.g. 100664).
    fn write_tree(&self, entries: &BTreeMap<Vec<u8>, FixtureEntry>) -> Result<String> {
        // dir -> name -> (mode, id); directories get filled in deepest-first.
        let mut dirs: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, FixtureEntry>> = BTreeMap::new();
        dirs.insert(Vec::new(), BTreeMap::new());
        for (path, e) in entries {
            let (dir, name) = match path.iter().rposition(|b| *b == b'/') {
                Some(i) => (path[..i].to_vec(), path[i + 1..].to_vec()),
                None => (Vec::new(), path.clone()),
            };
            dirs.entry(dir.clone()).or_default().insert(name, e.clone());
            let mut d = dir;
            while !d.is_empty() {
                let parent = match d.iter().rposition(|b| *b == b'/') {
                    Some(i) => d[..i].to_vec(),
                    None => Vec::new(),
                };
                dirs.entry(parent.clone()).or_default();
                d = parent;
            }
        }
        let mut order: Vec<Vec<u8>> = dirs.keys().cloned().collect();
        order.sort_by_key(|d| {
            std::cmp::Reverse(if d.is_empty() {
                0
            } else {
                d.iter().filter(|b| **b == b'/').count() + 1
            })
        });
        let mut tree_ids: BTreeMap<Vec<u8>, String> = BTreeMap::new();
        for dir in order {
            let mut input: Vec<u8> = Vec::new();
            if let Some(files) = dirs.get(&dir) {
                for (name, (mode, oid)) in files {
                    let kind = if *mode == 0o160000 { "commit" } else { "blob" };
                    input.extend_from_slice(format!("{mode:06o} {kind} {oid}\t").as_bytes());
                    input.extend_from_slice(name);
                    input.push(0);
                }
            }
            for (sub, id) in &tree_ids {
                let parent_and_name = match sub.iter().rposition(|b| *b == b'/') {
                    Some(i) => (&sub[..i], &sub[i + 1..]),
                    None => (&[][..], &sub[..]),
                };
                if parent_and_name.0 == dir.as_slice() {
                    input.extend_from_slice(format!("040000 tree {id}\t").as_bytes());
                    input.extend_from_slice(parent_and_name.1);
                    input.push(0);
                }
            }
            let id = self.git_line(&["mktree", "-z"], Some(&input))?;
            tree_ids.insert(dir, id);
        }
        tree_ids
            .get(&Vec::new())
            .cloned()
            .ok_or_else(|| invalid("no root tree"))
    }

    fn commit(
        &mut self,
        branch: &str,
        tree: &str,
        parents: &[String],
        message: &str,
    ) -> Result<String> {
        self.clock += 60;
        let mut args: Vec<String> = vec!["commit-tree".into(), tree.into()];
        for p in parents {
            args.push("-p".into());
            args.push(p.clone());
        }
        let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let mut msg = message.as_bytes().to_vec();
        if !msg.ends_with(b"\n") {
            msg.push(b'\n');
        }
        let id = self.git_line(&argv, Some(&msg))?;
        let refname = format!("refs/heads/{branch}");
        self.git(&["update-ref", &refname, &id], None)?;
        Ok(id)
    }
}

impl GitFixture {
    /// Run `script` with the git CLI (callers skip when [`git_available`] is false).
    ///
    /// Semantics (deterministic: fixed identities, dates advancing 60 s per commit from
    /// 2023-11-14, UTC):
    /// - `Commit` starts from the tip of `branch`; a branch that does not exist yet starts
    ///   from the branch used by the previous step (a fork), or empty after `OrphanRoot` or in
    ///   an empty repository (a root commit). Files are written with their exact git mode
    ///   (100644, 100755, 120000 with the target as content, or 100664); trees are built with
    ///   `git mktree`, so modes are never normalized.
    /// - `Merge` commits on `into` with parents `[into, from...]` (octopus for 2+ `from`).
    ///   The snapshot is a file-level three-way merge against `git merge-base`: one-sided
    ///   changes are taken, and a path changed on both sides keeps the first parent's version.
    /// - `OrphanRoot` makes the next `Commit`/`EmptyCommit` on `branch` a parentless commit.
    /// - `EmptyCommit` reuses the tip's tree (or the empty tree for a new root).
    ///
    /// SPEC 19.14 (fixture histories: octopus, roots, empty commits, modes, gitlinks).
    pub fn build(script: &[GitOp]) -> Result<Self> {
        if !git_available() {
            return Err(unavailable("the git CLI is not available"));
        }
        let dir = tempfile::TempDir::new().map_err(|e| unavailable(format!("tempdir: {e}")))?;
        let mut run = Runner {
            dir: dir.path(),
            clock: 1_700_000_000,
        };
        run.git(&["init", "-q", "-b", "main", "."], None)?;
        run.git(&["config", "core.autocrlf", "false"], None)?;
        run.git(&["config", "core.fileMode", "false"], None)?;
        let mut orphans: BTreeSet<String> = BTreeSet::new();
        let mut current: Option<String> = None;
        for op in script {
            match op {
                GitOp::Commit {
                    branch,
                    files,
                    deletes,
                    gitlinks,
                    message,
                } => {
                    let parent = start_point(&run, branch, &mut orphans, current.as_deref())?;
                    let mut entries = run.entries(parent.as_deref())?;
                    for d in deletes {
                        let d = d.as_bytes();
                        let mut prefix = d.to_vec();
                        prefix.push(b'/');
                        entries.retain(|p, _| p.as_slice() != d && !p.starts_with(&prefix));
                    }
                    for (path, mode, content) in files {
                        let oid = run.git_line(&["hash-object", "-w", "--stdin"], Some(content))?;
                        remove_conflicts(&mut entries, path.as_bytes());
                        entries.insert(path.as_bytes().to_vec(), (*mode, oid));
                    }
                    for (path, sha) in gitlinks {
                        let hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
                        remove_conflicts(&mut entries, path.as_bytes());
                        entries.insert(path.as_bytes().to_vec(), (0o160000, hex));
                    }
                    let tree = run.write_tree(&entries)?;
                    let parents: Vec<String> = parent.into_iter().collect();
                    run.commit(branch, &tree, &parents, message)?;
                    current = Some(branch.clone());
                }
                GitOp::EmptyCommit { branch, message } => {
                    let parent = start_point(&run, branch, &mut orphans, current.as_deref())?;
                    let tree = match &parent {
                        Some(p) => run.git_line(&["rev-parse", &format!("{p}^{{tree}}")], None)?,
                        None => run.write_tree(&BTreeMap::new())?,
                    };
                    let parents: Vec<String> = parent.into_iter().collect();
                    run.commit(branch, &tree, &parents, message)?;
                    current = Some(branch.clone());
                }
                GitOp::OrphanRoot { branch } => {
                    orphans.insert(branch.clone());
                    current = Some(branch.clone());
                }
                GitOp::Merge {
                    into,
                    from,
                    message,
                } => {
                    let ours = run
                        .tip(into)?
                        .ok_or_else(|| invalid(format!("merge into unknown branch {into}")))?;
                    let mut parents = vec![ours.clone()];
                    let mut merged = run.entries(Some(&ours))?;
                    for f in from {
                        let theirs = run
                            .tip(f)?
                            .ok_or_else(|| invalid(format!("merge of unknown branch {f}")))?;
                        let base = run
                            .git(&["merge-base", &ours, &theirs], None)
                            .ok()
                            .map(|o| String::from_utf8_lossy(&o).trim().to_string())
                            .filter(|s| !s.is_empty());
                        let base_entries = run.entries(base.as_deref())?;
                        let their_entries = run.entries(Some(&theirs))?;
                        let paths: BTreeSet<&Vec<u8>> =
                            base_entries.keys().chain(their_entries.keys()).collect();
                        let mut updates = Vec::new();
                        for p in paths {
                            let b = base_entries.get(p);
                            let t = their_entries.get(p);
                            if b != t && merged.get(p) == b {
                                updates.push((p.clone(), t.cloned()));
                            }
                        }
                        for (p, t) in updates {
                            match t {
                                Some(t) => {
                                    remove_conflicts(&mut merged, &p);
                                    merged.insert(p, t);
                                }
                                None => {
                                    merged.remove(&p);
                                }
                            }
                        }
                        parents.push(theirs);
                    }
                    let tree = run.write_tree(&merged)?;
                    run.commit(into, &tree, &parents, message)?;
                    current = Some(into.clone());
                }
            }
        }
        // Point HEAD at main when it exists, else at the last branch used.
        let head = if run.tip("main")?.is_some() {
            Some("main".to_string())
        } else {
            current
        };
        if let Some(h) = head {
            run.git(&["symbolic-ref", "HEAD", &format!("refs/heads/{h}")], None)?;
        }
        Ok(GitFixture { dir })
    }

    /// The repository directory.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Resolve `rev` (e.g. a branch name) to a 40-hex commit id with the git CLI.
    pub fn rev_parse(&self, rev: &str) -> Result<String> {
        let run = Runner {
            dir: self.dir.path(),
            clock: 0,
        };
        run.git_line(&["rev-parse", "--verify", rev], None)
    }

    /// Write a full-history bundle of this fixture to `out` (`git bundle create --all`).
    pub fn bundle(&self, out: &Path) -> Result<()> {
        let run = Runner {
            dir: self.dir.path(),
            clock: 0,
        };
        let out = out.to_string_lossy().to_string();
        run.git(&["bundle", "create", &out, "--all"], None)
            .map(|_| ())
    }
}

/// Remove entries that would collide with a file at `path` (the path as a directory, or any
/// ancestor of `path` as a file).
fn remove_conflicts(entries: &mut BTreeMap<Vec<u8>, FixtureEntry>, path: &[u8]) {
    let mut prefix = path.to_vec();
    prefix.push(b'/');
    entries.retain(|p, _| !p.starts_with(&prefix));
    for (i, b) in path.iter().enumerate() {
        if *b == b'/' {
            entries.remove(&path[..i]);
        }
    }
}

/// The parent of the next commit on `branch` (see [`GitFixture::build`]).
fn start_point(
    run: &Runner<'_>,
    branch: &str,
    orphans: &mut BTreeSet<String>,
    current: Option<&str>,
) -> Result<Option<String>> {
    if let Some(tip) = run.tip(branch)? {
        return Ok(Some(tip));
    }
    if orphans.remove(branch) {
        return Ok(None);
    }
    match current {
        Some(c) if c != branch => run.tip(c),
        _ => Ok(None),
    }
}

/// One fixture step.
#[derive(Clone, Debug)]
pub enum GitOp {
    /// Commit on `branch` writing `files` as `(path, git mode, content)`, removing `deletes`,
    /// and adding `gitlinks` as `(path, 20-byte sha)`.
    Commit {
        /// Branch.
        branch: String,
        /// Files to write.
        files: Vec<(String, u32, Vec<u8>)>,
        /// Paths to delete.
        deletes: Vec<String>,
        /// Gitlinks to add.
        gitlinks: Vec<(String, [u8; 20])>,
        /// Message.
        message: String,
    },
    /// Merge `from` branches into `into` (more than one = octopus).
    Merge {
        /// Target branch.
        into: String,
        /// Merged branches.
        from: Vec<String>,
        /// Message.
        message: String,
    },
    /// Start an orphan branch (a second root).
    OrphanRoot {
        /// Branch.
        branch: String,
    },
    /// An empty commit.
    EmptyCommit {
        /// Branch.
        branch: String,
        /// Message.
        message: String,
    },
}
