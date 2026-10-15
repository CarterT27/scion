//! Safe filesystem access under the working-copy root (SPEC 11.3, 17; P11).
//!
//! Every path below the root is resolved one component at a time with `openat` and
//! `O_NOFOLLOW | O_DIRECTORY`, starting from a directory descriptor of the root. A pre-existing
//! symbolic link (or a non-directory) in a parent position is refused, never followed, so no
//! operation of [`SafeRoot`] can reach outside the root (SPEC 11.3, 17). Files are written to a
//! temporary name in the target directory and then `renameat`-ed over the final name, which
//! also replaces (rather than follows) a symlink at the final component.

use crate::index::StatTuple;
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{AtFlags, Mode as FsMode, OFlags};
use rustix::io::Errno;
use scion_core::{ErrorCode, Mode, Result, ScionError, subcode};
use scion_object::RepoPath;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Maximum path length in bytes (SPEC 4.4).
const MAX_PATH_BYTES: usize = 4096;
/// Maximum name length in bytes (SPEC 4.4).
const MAX_NAME_BYTES: usize = 255;
/// Prefix of temporary files created next to their final name.
pub(crate) const TEMP_PREFIX: &str = ".scion-tmp-";
/// Attempts at finding an unused temporary name.
const TEMP_ATTEMPTS: usize = 16;

/// The working-copy root opened with `O_NOFOLLOW` traversal: nothing is ever written outside
/// it and symlinked parents are refused (SPEC 11.3, 17).
pub struct SafeRoot {
    root: PathBuf,
    fd: OwnedFd,
    case_insensitive: OnceLock<bool>,
}

impl std::fmt::Debug for SafeRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SafeRoot")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

/// How a parent walk treats a missing directory.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Missing {
    /// Create missing directories (mode 0755).
    Create,
    /// Report the walk as absent.
    Absent,
}

/// Outcome of resolving the parents of a path.
enum Parents {
    /// Every parent exists: descriptors of each parent below the root (outermost first).
    Found(Vec<OwnedFd>),
    /// A parent is missing.
    Missing,
    /// A parent is a symlink or not a directory.
    Blocked,
}

impl SafeRoot {
    /// Open the root directory (SPEC 11.3). The root path itself may contain symlinks (for
    /// example `/tmp` on macOS); everything below it is resolved without following any.
    pub fn open(root: &Path) -> Result<Self> {
        let fd = rustix::fs::openat(
            rustix::fs::CWD,
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            FsMode::empty(),
        )
        .map_err(|e| errno_error("open working-copy root", root, e))?;
        Ok(SafeRoot {
            root: root.to_path_buf(),
            fd,
            case_insensitive: OnceLock::new(),
        })
    }

    /// Write via temp file plus rename in the target directory, with the executable bit for
    /// [`Mode::Executable`] (SPEC 11.3). Missing parent directories are created; a symlinked
    /// or non-directory parent is refused. [`Mode::Symlink`] delegates to [`SafeRoot::symlink`]
    /// with `data` as the target.
    pub fn write_file_atomic(&self, p: &RepoPath, data: &[u8], mode: Mode) -> Result<()> {
        self.write_file_atomic_raw(p.as_bytes(), data, mode)
    }

    /// Return the post-rename stat without walking the same parents a second time.
    pub(crate) fn write_file_atomic_with_stat(
        &self,
        p: &RepoPath,
        data: &[u8],
        mode: Mode,
    ) -> Result<StatTuple> {
        self.write_file_with_stat(p.as_bytes(), data, mode)
    }

    /// Create (or atomically replace) a symlink with the given target (SPEC 11.3).
    pub fn symlink(&self, p: &RepoPath, target: &[u8]) -> Result<()> {
        self.symlink_raw(p.as_bytes(), target)
    }

    /// Remove a file or symlink, pruning parent directories that became empty (never the
    /// root). Removing an absent path succeeds.
    pub fn remove(&self, p: &RepoPath) -> Result<()> {
        self.remove_raw(p.as_bytes())
    }

    /// Read a regular file (refusing symlinks anywhere on the path).
    pub fn read(&self, p: &RepoPath) -> Result<Vec<u8>> {
        self.read_raw(p.as_bytes())
    }

    /// Stat without following symlinks; `None` when absent or when a parent component is not
    /// a real directory (SPEC 11.5).
    pub fn stat(&self, p: &RepoPath) -> Result<Option<StatTuple>> {
        self.stat_raw(p.as_bytes())
    }

    /// Read a symlink target.
    pub fn read_link(&self, p: &RepoPath) -> Result<Vec<u8>> {
        self.read_link_raw(p.as_bytes())
    }

    /// Whether the filesystem is case-insensitive (probed once, with a temporary file in the
    /// root; PLAN 6.17).
    pub fn is_case_insensitive(&self) -> bool {
        *self.case_insensitive.get_or_init(|| self.probe_case())
    }

    /// The root path as given to [`SafeRoot::open`].
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Create a directory (and its missing parents) below the root, refusing symlinked
    /// parents (SPEC 11.2: directories on the way to a scoped prefix are created).
    pub fn create_dir_all(&self, p: &RepoPath) -> Result<()> {
        self.create_dir_all_raw(p.as_bytes())
    }

    /// The root directory descriptor (for the status walk).
    pub(crate) fn root_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    // ---- byte-level implementations -------------------------------------------------------

    pub(crate) fn create_dir_all_raw(&self, p: &[u8]) -> Result<()> {
        let comps = components(p)?;
        let mut chain: Vec<OwnedFd> = Vec::new();
        for comp in comps {
            let fd = {
                let dir = chain.last().map(|f| f.as_fd()).unwrap_or(self.fd.as_fd());
                open_or_create_dir(dir, comp)?
            };
            chain.push(fd);
        }
        Ok(())
    }

    pub(crate) fn write_file_atomic_raw(&self, p: &[u8], data: &[u8], mode: Mode) -> Result<()> {
        self.write_file_with_stat(p, data, mode).map(|_| ())
    }

    fn write_file_with_stat(&self, p: &[u8], data: &[u8], mode: Mode) -> Result<StatTuple> {
        let perm: u16 = match mode {
            Mode::File => 0o644,
            Mode::Executable => 0o755,
            Mode::Symlink => return self.symlink_with_stat(p, data),
            Mode::Tree => {
                return Err(ScionError::invalid(
                    subcode::INVALID_MODE,
                    "cannot write a tree entry as a file",
                ));
            }
        };
        let (chain, leaf) = self.parents_for_write(p)?;
        let dir = chain.last().map(|f| f.as_fd()).unwrap_or(self.fd.as_fd());
        let (tmp, fd) = create_temp(dir, perm)?;
        let result = (|| -> Result<StatTuple> {
            let mut file = std::fs::File::from(fd);
            file.write_all(data)
                .map_err(|e| io_error("write temporary file", e))?;
            // The creation mode is filtered by the umask; set the exact bits.
            rustix::fs::fchmod(&file, fs_mode(perm))
                .map_err(|e| errno_plain("set file mode", e))?;
            rustix::fs::renameat(dir, tmp.as_slice(), dir, leaf)
                .map_err(|e| rename_error(e, "file"))?;
            let stat = rustix::fs::fstat(&file).map_err(|e| errno_plain("stat written file", e))?;
            Ok(tuple_from_stat(&stat))
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(dir, tmp.as_slice(), AtFlags::empty());
        }
        result
    }

    pub(crate) fn symlink_raw(&self, p: &[u8], target: &[u8]) -> Result<()> {
        self.symlink_with_stat(p, target).map(|_| ())
    }

    fn symlink_with_stat(&self, p: &[u8], target: &[u8]) -> Result<StatTuple> {
        if target.is_empty() || target.contains(&0) {
            return Err(ScionError::invalid(
                subcode::INVALID_PATH,
                "symlink target must be non-empty and contain no NUL byte",
            ));
        }
        let (chain, leaf) = self.parents_for_write(p)?;
        let dir = chain.last().map(|f| f.as_fd()).unwrap_or(self.fd.as_fd());
        let mut created = None;
        for _ in 0..TEMP_ATTEMPTS {
            let tmp = temp_name();
            match rustix::fs::symlinkat(target, dir, tmp.as_slice()) {
                Ok(()) => {
                    created = Some(tmp);
                    break;
                }
                Err(Errno::EXIST) => continue,
                Err(e) => return Err(errno_plain("create symlink", e)),
            }
        }
        let Some(tmp) = created else {
            return Err(ScionError::new(
                ErrorCode::Unavailable,
                "could not find an unused temporary name",
            ));
        };
        let result = rustix::fs::renameat(dir, tmp.as_slice(), dir, leaf)
            .map_err(|e| rename_error(e, "symlink"))
            .and_then(|()| {
                stat_at(dir, leaf)?
                    .map(|st| tuple_from_stat(&st))
                    .ok_or_else(|| ScionError::not_found("written symlink disappeared"))
            });
        if result.is_err() {
            let _ = rustix::fs::unlinkat(dir, tmp.as_slice(), AtFlags::empty());
        }
        result
    }

    pub(crate) fn remove_raw(&self, p: &[u8]) -> Result<()> {
        let comps = components(p)?;
        let Some((leaf, parents)) = comps.split_last() else {
            return Err(root_refused());
        };
        let chain = match self.walk_parents(parents, Missing::Absent)? {
            Parents::Found(chain) => chain,
            Parents::Missing => return Ok(()),
            Parents::Blocked => return Err(blocked_error()),
        };
        {
            let dir = chain.last().map(|f| f.as_fd()).unwrap_or(self.fd.as_fd());
            match rustix::fs::unlinkat(dir, *leaf, AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(Errno::ISDIR) | Err(Errno::PERM) => {
                    return Err(ScionError::invalid(
                        subcode::FILE_DIRECTORY,
                        "cannot remove a directory as a file",
                    ));
                }
                Err(e) => return Err(errno_plain("remove file", e)),
            }
        }
        // Prune parents that became empty, innermost first; stop at the first non-empty one.
        for i in (0..chain.len()).rev() {
            let parent = if i == 0 {
                self.fd.as_fd()
            } else {
                chain[i - 1].as_fd()
            };
            if rustix::fs::unlinkat(parent, parents[i], AtFlags::REMOVEDIR).is_err() {
                break;
            }
        }
        Ok(())
    }

    pub(crate) fn read_raw(&self, p: &[u8]) -> Result<Vec<u8>> {
        let comps = components(p)?;
        let Some((leaf, parents)) = comps.split_last() else {
            return Err(root_refused());
        };
        let chain = match self.walk_parents(parents, Missing::Absent)? {
            Parents::Found(chain) => chain,
            Parents::Missing => return Err(ScionError::not_found("file not found")),
            Parents::Blocked => return Err(blocked_error()),
        };
        let dir = chain.last().map(|f| f.as_fd()).unwrap_or(self.fd.as_fd());
        read_file_at(dir, leaf)
    }

    pub(crate) fn stat_raw(&self, p: &[u8]) -> Result<Option<StatTuple>> {
        let comps = components(p)?;
        let Some((leaf, parents)) = comps.split_last() else {
            let st = rustix::fs::fstat(&self.fd).map_err(|e| errno_plain("stat root", e))?;
            return Ok(Some(tuple_from_stat(&st)));
        };
        let chain = match self.walk_parents(parents, Missing::Absent)? {
            Parents::Found(chain) => chain,
            Parents::Missing | Parents::Blocked => return Ok(None),
        };
        let dir = chain.last().map(|f| f.as_fd()).unwrap_or(self.fd.as_fd());
        Ok(stat_at(dir, leaf)?.map(|st| tuple_from_stat(&st)))
    }

    pub(crate) fn read_link_raw(&self, p: &[u8]) -> Result<Vec<u8>> {
        let comps = components(p)?;
        let Some((leaf, parents)) = comps.split_last() else {
            return Err(root_refused());
        };
        let chain = match self.walk_parents(parents, Missing::Absent)? {
            Parents::Found(chain) => chain,
            Parents::Missing => return Err(ScionError::not_found("symlink not found")),
            Parents::Blocked => return Err(blocked_error()),
        };
        let dir = chain.last().map(|f| f.as_fd()).unwrap_or(self.fd.as_fd());
        readlink_at(dir, leaf)
    }

    /// Resolve (creating) the parents of `p` for a write; returns the chain and the leaf.
    fn parents_for_write<'a>(&self, p: &'a [u8]) -> Result<(Vec<OwnedFd>, &'a [u8])> {
        let comps = components(p)?;
        let Some((leaf, parents)) = comps.split_last() else {
            return Err(root_refused());
        };
        match self.walk_parents(parents, Missing::Create)? {
            Parents::Found(chain) => Ok((chain, leaf)),
            Parents::Missing | Parents::Blocked => Err(blocked_error()),
        }
    }

    /// Open every parent component with `O_NOFOLLOW | O_DIRECTORY`.
    fn walk_parents(&self, parents: &[&[u8]], missing: Missing) -> Result<Parents> {
        let mut chain: Vec<OwnedFd> = Vec::with_capacity(parents.len());
        for comp in parents {
            let opened = {
                let dir = chain.last().map(|f| f.as_fd()).unwrap_or(self.fd.as_fd());
                match open_dir_nofollow(dir, comp) {
                    Ok(fd) => fd,
                    Err(Errno::NOENT) if missing == Missing::Create => {
                        open_or_create_dir(dir, comp)?
                    }
                    Err(Errno::NOENT) => return Ok(Parents::Missing),
                    Err(Errno::LOOP) | Err(Errno::NOTDIR) => return Ok(Parents::Blocked),
                    Err(e) => return Err(errno_plain("open parent directory", e)),
                }
            };
            chain.push(opened);
        }
        Ok(Parents::Found(chain))
    }

    fn probe_case(&self) -> bool {
        let dir = self.fd.as_fd();
        for _ in 0..TEMP_ATTEMPTS {
            let name = format!(".scion-case-probe-{:016x}", rand::random::<u64>());
            match rustix::fs::openat(
                dir,
                name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                FsMode::from_bits_truncate(0o600),
            ) {
                Ok(fd) => {
                    drop(fd);
                    let upper = name.to_ascii_uppercase();
                    let insensitive =
                        rustix::fs::statat(dir, upper.as_str(), AtFlags::SYMLINK_NOFOLLOW).is_ok();
                    let _ = rustix::fs::unlinkat(dir, name.as_str(), AtFlags::empty());
                    tracing::debug!(
                        insensitive,
                        "probed working-copy filesystem case sensitivity"
                    );
                    return insensitive;
                }
                Err(Errno::EXIST) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "case-sensitivity probe failed; assuming case-sensitive");
                    return false;
                }
            }
        }
        false
    }
}

/// SPEC 4.4 plus never `.scion` in any case: validate a tree entry path before any write
/// (SPEC 17 checkout path safety). Errors are INVALID_ARGUMENT/invalid_path.
pub fn validate_entry_path(p: &[u8]) -> Result<RepoPath> {
    check_entry_path(p)?;
    RepoPath::parse(p)
}

/// The validation of [`validate_entry_path`] without constructing a [`RepoPath`]: a non-empty
/// path of at most 4096 bytes, no leading or trailing `/`, every name 1-255 bytes with no NUL,
/// not `.` or `..`, and not `.scion` under ASCII case-insensitive comparison (SPEC 4.4).
pub fn check_entry_path(p: &[u8]) -> Result<()> {
    if p.is_empty() {
        return Err(invalid_path("empty entry path"));
    }
    if p.len() > MAX_PATH_BYTES {
        return Err(invalid_path("path longer than 4096 bytes"));
    }
    for name in p.split(|b| *b == b'/') {
        check_name(name)?;
    }
    Ok(())
}

/// SPEC 4.4 name rule (plus `.scion` in any case).
pub(crate) fn check_name(name: &[u8]) -> Result<()> {
    if name.is_empty() {
        return Err(invalid_path(
            "empty path component (leading, trailing or doubled '/')",
        ));
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(invalid_path("path component longer than 255 bytes"));
    }
    if name.contains(&0) {
        return Err(invalid_path("path contains a NUL byte"));
    }
    if name == b"." || name == b".." {
        return Err(invalid_path("path contains a '.' or '..' component"));
    }
    if name.eq_ignore_ascii_case(b".scion") {
        return Err(invalid_path("path contains a '.scion' component"));
    }
    Ok(())
}

/// ASCII case-fold collisions among `paths` (deterministic on any filesystem; SPEC 4.4,
/// PLAN 6.17). Two distinct paths collide when they fold to the same path, or when one of
/// their directory prefixes folds equal while differing in case (so they would share a
/// directory on a case-insensitive filesystem). Each colliding path is reported once, paired
/// with the bytewise-smallest earlier path it collides with; pairs are `(smaller, larger)` and
/// sorted.
pub fn case_collisions(paths: &[RepoPath]) -> Vec<(RepoPath, RepoPath)> {
    let raw: Vec<&[u8]> = paths.iter().map(|p| p.as_bytes()).collect();
    collision_indices(&raw)
        .into_iter()
        .map(|(a, b)| (paths[a].clone(), paths[b].clone()))
        .collect()
}

/// Byte-level core of [`case_collisions`]: index pairs into `paths`.
pub(crate) fn collision_indices(paths: &[&[u8]]) -> Vec<(usize, usize)> {
    use std::collections::BTreeMap;
    let mut order: Vec<usize> = (0..paths.len()).collect();
    order.sort_by(|a, b| paths[*a].cmp(paths[*b]).then(a.cmp(b)));
    order.dedup_by(|a, b| paths[*a] == paths[*b]);
    // folded prefix -> (raw prefix, index of the first path that introduced it)
    let mut seen: BTreeMap<Vec<u8>, (&[u8], usize)> = BTreeMap::new();
    let mut out = Vec::new();
    for &i in &order {
        let p = paths[i];
        let mut ends: Vec<usize> = p
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == b'/')
            .map(|(k, _)| k)
            .collect();
        ends.push(p.len());
        for end in ends {
            let prefix = &p[..end];
            let folded = prefix.to_ascii_lowercase();
            match seen.get(&folded) {
                Some((raw, first)) if *raw != prefix => {
                    out.push((*first, i));
                    break;
                }
                Some(_) => {}
                None => {
                    seen.insert(folded, (prefix, i));
                }
            }
        }
    }
    out.sort_by(|a, b| (paths[a.0], paths[a.1]).cmp(&(paths[b.0], paths[b.1])));
    out
}

// ---- descriptor-level helpers (shared with the status walk) ------------------------------

/// Split a path into validated components (the root has none).
fn components(p: &[u8]) -> Result<Vec<&[u8]>> {
    if p.is_empty() {
        return Ok(Vec::new());
    }
    if p.len() > MAX_PATH_BYTES {
        return Err(invalid_path("path longer than 4096 bytes"));
    }
    let comps: Vec<&[u8]> = p.split(|b| *b == b'/').collect();
    for c in &comps {
        check_name(c)?;
    }
    Ok(comps)
}

/// Open a subdirectory without following a symlink.
pub(crate) fn open_dir_nofollow(
    dir: BorrowedFd<'_>,
    name: &[u8],
) -> std::result::Result<OwnedFd, Errno> {
    rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        FsMode::empty(),
    )
}

/// Open a subdirectory, creating it (0755) when missing; symlinks are refused.
fn open_or_create_dir(dir: BorrowedFd<'_>, name: &[u8]) -> Result<OwnedFd> {
    match open_dir_nofollow(dir, name) {
        Ok(fd) => return Ok(fd),
        Err(Errno::NOENT) => {}
        Err(Errno::LOOP) | Err(Errno::NOTDIR) => return Err(blocked_error()),
        Err(e) => return Err(errno_plain("open directory", e)),
    }
    match rustix::fs::mkdirat(dir, name, FsMode::from_bits_truncate(0o755)) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(e) => return Err(errno_plain("create directory", e)),
    }
    open_dir_nofollow(dir, name).map_err(|e| match e {
        Errno::LOOP | Errno::NOTDIR => blocked_error(),
        e => errno_plain("open directory", e),
    })
}

/// The names in a directory (without `.` and `..`), sorted bytewise.
pub(crate) fn list_dir(dir: BorrowedFd<'_>) -> Result<Vec<Vec<u8>>> {
    let mut d = rustix::fs::Dir::read_from(dir).map_err(|e| errno_plain("read directory", e))?;
    let mut names = Vec::new();
    while let Some(entry) = d.read() {
        let entry = entry.map_err(|e| errno_plain("read directory", e))?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        names.push(name.to_vec());
    }
    names.sort();
    Ok(names)
}

/// `lstat` relative to a directory; `None` when absent.
pub(crate) fn stat_at(dir: BorrowedFd<'_>, name: &[u8]) -> Result<Option<rustix::fs::Stat>> {
    match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => Ok(Some(st)),
        Err(Errno::NOENT) | Err(Errno::NOTDIR) => Ok(None),
        Err(e) => Err(errno_plain("stat", e)),
    }
}

/// Read a regular file relative to a directory, refusing symlinks and non-regular files.
pub(crate) fn read_file_at(dir: BorrowedFd<'_>, name: &[u8]) -> Result<Vec<u8>> {
    let fd = rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        FsMode::empty(),
    )
    .map_err(|e| match e {
        Errno::NOENT | Errno::NOTDIR => ScionError::not_found("file not found"),
        Errno::LOOP => ScionError::invalid(subcode::INVALID_PATH, "path is a symbolic link"),
        e => errno_plain("open file", e),
    })?;
    let st = rustix::fs::fstat(&fd).map_err(|e| errno_plain("stat file", e))?;
    if !tuple_from_stat(&st).is_regular() {
        return Err(ScionError::invalid(
            subcode::FILE_DIRECTORY,
            "path is not a regular file",
        ));
    }
    let mut file = std::fs::File::from(fd);
    let mut out = Vec::with_capacity(usize::try_from(st.st_size).unwrap_or(0));
    file.read_to_end(&mut out)
        .map_err(|e| io_error("read file", e))?;
    Ok(out)
}

/// Read a symlink target relative to a directory.
pub(crate) fn readlink_at(dir: BorrowedFd<'_>, name: &[u8]) -> Result<Vec<u8>> {
    match rustix::fs::readlinkat(dir, name, Vec::new()) {
        Ok(target) => Ok(target.into_bytes()),
        Err(Errno::NOENT) | Err(Errno::NOTDIR) => Err(ScionError::not_found("symlink not found")),
        Err(Errno::INVAL) => Err(ScionError::invalid(
            subcode::INVALID_MODE,
            "path is not a symbolic link",
        )),
        Err(e) => Err(errno_plain("read symlink", e)),
    }
}

/// Convert a raw `stat` into the index tuple (SPEC 11.5).
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
pub(crate) fn tuple_from_stat(st: &rustix::fs::Stat) -> StatTuple {
    let sec_ns = |s: i64, ns: i64| i128::from(s) * 1_000_000_000 + i128::from(ns);
    StatTuple {
        size: u64::try_from(st.st_size).unwrap_or(0),
        mtime_ns: sec_ns(st.st_mtime as i64, st.st_mtime_nsec as i64),
        ctime_ns: sec_ns(st.st_ctime as i64, st.st_ctime_nsec as i64),
        inode: st.st_ino as u64,
        mode: st.st_mode as u32,
    }
}

/// Permission bits as a rustix mode (`mode_t` is `u16` on macOS, `u32` on Linux).
#[allow(clippy::useless_conversion)]
fn fs_mode(perm: u16) -> FsMode {
    FsMode::from_bits_truncate(perm.into())
}

/// Create a fresh temporary file in `dir`.
fn create_temp(dir: BorrowedFd<'_>, perm: u16) -> Result<(Vec<u8>, OwnedFd)> {
    for _ in 0..TEMP_ATTEMPTS {
        let name = temp_name();
        match rustix::fs::openat(
            dir,
            name.as_slice(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            fs_mode(perm),
        ) {
            Ok(fd) => return Ok((name, fd)),
            Err(Errno::EXIST) => continue,
            Err(e) => return Err(errno_plain("create temporary file", e)),
        }
    }
    Err(ScionError::new(
        ErrorCode::Unavailable,
        "could not find an unused temporary name",
    ))
}

fn temp_name() -> Vec<u8> {
    format!("{TEMP_PREFIX}{:016x}", rand::random::<u64>()).into_bytes()
}

/// Write `data` to `dir/name` atomically (temp file in `dir`, optional fsync, rename) with the
/// given permission bits. Used for the files under `.scion/` and the credentials file.
pub(crate) fn write_atomic(
    dir: &Path,
    name: &str,
    data: &[u8],
    perm: u32,
    sync: bool,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{name}.tmp-"))
        .tempfile_in(dir)
        .map_err(|e| io_error("create temporary file", e))?;
    tmp.as_file_mut()
        .write_all(data)
        .map_err(|e| io_error("write temporary file", e))?;
    tmp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(perm))
        .map_err(|e| io_error("set file mode", e))?;
    if sync {
        tmp.as_file()
            .sync_all()
            .map_err(|e| io_error("sync temporary file", e))?;
    }
    tmp.persist(dir.join(name))
        .map_err(|e| io_error("rename temporary file", e.error))?;
    Ok(())
}

// ---- errors ------------------------------------------------------------------------------

fn invalid_path(msg: &str) -> ScionError {
    ScionError::invalid(subcode::INVALID_PATH, msg)
}

fn root_refused() -> ScionError {
    invalid_path("operation not allowed on the working-copy root")
}

fn blocked_error() -> ScionError {
    invalid_path(
        "a parent path component is a symbolic link or not a directory; refusing to follow it",
    )
}

fn rename_error(e: Errno, what: &str) -> ScionError {
    match e {
        Errno::ISDIR | Errno::NOTEMPTY | Errno::EXIST | Errno::NOTDIR => ScionError::invalid(
            subcode::FILE_DIRECTORY,
            format!("cannot replace a directory with a {what}"),
        ),
        e => errno_plain("rename temporary file", e),
    }
}

/// Map an OS error to a [`ScionError`] (the message never carries repository paths).
pub(crate) fn io_error(what: &str, e: std::io::Error) -> ScionError {
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => ErrorCode::NotFound,
        std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        _ => ErrorCode::Unavailable,
    };
    ScionError::new(code, format!("{what}: {e}"))
}

fn errno_plain(what: &str, e: Errno) -> ScionError {
    io_error(what, std::io::Error::from(e))
}

fn errno_error(what: &str, path: &Path, e: Errno) -> ScionError {
    io_error(
        &format!("{what} {}", path.display()),
        std::io::Error::from(e),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn root() -> (tempfile::TempDir, SafeRoot) {
        let dir = tempfile::tempdir().unwrap();
        let r = SafeRoot::open(dir.path()).unwrap();
        (dir, r)
    }

    #[test]
    fn atomic_write_returns_the_post_rename_stat_for_each_mode() {
        let (_dir, root) = root();
        let path = RepoPath::parse(b"nested/file").unwrap();
        for mode in [Mode::File, Mode::Executable, Mode::Symlink, Mode::File] {
            let written = root
                .write_file_atomic_with_stat(&path, b"target", mode)
                .unwrap();
            assert_eq!(Some(written), root.stat(&path).unwrap());
        }
        let blocked = RepoPath::parse(b"nested/file/escape").unwrap();
        assert!(
            root.write_file_atomic_with_stat(&blocked, b"no", Mode::File)
                .is_err()
        );
    }

    #[test]
    fn entry_path_rules() {
        for ok in [
            &b"a"[..],
            b"a/b/c",
            b".scionx",
            b"x/.scion2",
            b"\xff\xfe",
            b"..a",
            b"a..",
        ] {
            check_entry_path(ok).unwrap();
        }
        let long_name = vec![b'n'; 256];
        let long_path = [&vec![b'a'; 200][..], b"/"].concat().repeat(21);
        for bad in [
            &b""[..],
            b"..",
            b".",
            b"a/../b",
            b"a/./b",
            b".scion",
            b".SCION",
            b"a/.Scion/b",
            b"a\0b",
            b"/a",
            b"a/",
            b"a//b",
            &long_name,
            &long_path[..long_path.len() - 1],
        ] {
            let err = check_entry_path(bad).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgument);
            assert_eq!(err.subcode(), Some(subcode::INVALID_PATH));
            // validate_entry_path refuses before constructing a RepoPath.
            assert!(validate_entry_path(bad).is_err());
        }
        check_entry_path(&vec![b'n'; 255]).unwrap();
    }

    #[test]
    fn collisions_bytes() {
        let ps: Vec<&[u8]> = vec![
            b"README", b"readme", b"src/a", b"Src/b", b"x/Y", b"x/y", b"ok", b"ok",
        ];
        let got = collision_indices(&ps);
        let named: Vec<(&[u8], &[u8])> = got.iter().map(|(a, b)| (ps[*a], ps[*b])).collect();
        assert_eq!(
            named,
            vec![
                (&b"README"[..], &b"readme"[..]),
                (&b"Src/b"[..], &b"src/a"[..]),
                (&b"x/Y"[..], &b"x/y"[..]),
            ]
        );
        // File vs directory differing in case.
        let ps: Vec<&[u8]> = vec![b"A", b"a/b"];
        assert_eq!(collision_indices(&ps), vec![(0, 1)]);
        // Same-case file/dir prefixes and non-ASCII bytes never collide.
        let ps: Vec<&[u8]> = vec![b"a/b", b"a/c", b"\xc3\x84", b"\xc3\xa4"];
        assert!(collision_indices(&ps).is_empty());
    }

    #[test]
    fn write_read_stat_remove() {
        let (dir, r) = root();
        r.write_file_atomic_raw(b"d/e/f", b"hello", Mode::File)
            .unwrap();
        assert_eq!(std::fs::read(dir.path().join("d/e/f")).unwrap(), b"hello");
        assert_eq!(r.read_raw(b"d/e/f").unwrap(), b"hello");
        let st = r.stat_raw(b"d/e/f").unwrap().unwrap();
        assert!(st.is_regular() && !st.is_executable());
        assert_eq!(st.size, 5);
        let md = std::fs::metadata(dir.path().join("d/e/f")).unwrap();
        assert_eq!(md.permissions().mode() & 0o777, 0o644);

        r.write_file_atomic_raw(b"d/e/f", b"#!/bin/sh\n", Mode::Executable)
            .unwrap();
        let md = std::fs::metadata(dir.path().join("d/e/f")).unwrap();
        assert_eq!(md.permissions().mode() & 0o777, 0o755);
        assert!(r.stat_raw(b"d/e/f").unwrap().unwrap().is_executable());

        r.write_file_atomic_raw(b"d/link", b"e/f", Mode::Symlink)
            .unwrap();
        assert_eq!(r.read_link_raw(b"d/link").unwrap(), b"e/f");
        assert!(r.stat_raw(b"d/link").unwrap().unwrap().is_symlink());
        assert!(r.read_raw(b"d/link").is_err());
        assert!(r.read_link_raw(b"d/e/f").is_err());

        assert_eq!(r.stat_raw(b"nope/x").unwrap(), None);
        assert_eq!(r.stat_raw(b"d/nope").unwrap(), None);
        assert_eq!(r.read_raw(b"d/nope").unwrap_err().code, ErrorCode::NotFound);

        r.remove_raw(b"d/e/f").unwrap();
        assert!(!dir.path().join("d/e").exists(), "empty parent pruned");
        assert!(dir.path().join("d/link").symlink_metadata().is_ok());
        r.remove_raw(b"d/link").unwrap();
        assert!(!dir.path().join("d").exists());
        assert!(dir.path().exists());
        r.remove_raw(b"d/link").unwrap();
        // No temporary files left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn symlinked_parent_is_refused() {
        let (dir, r) = root();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("a/inner")).unwrap();
        std::fs::write(outside.path().join("victim"), b"keep").unwrap();

        for p in [
            &b"link/x"[..],
            b"link/sub/x",
            b"a/inner/x",
            b"a/inner/deep/er/x",
        ] {
            let err = r.write_file_atomic_raw(p, b"evil", Mode::File).unwrap_err();
            assert_eq!(err.subcode(), Some(subcode::INVALID_PATH));
            assert!(r.symlink_raw(p, b"/etc/passwd").is_err());
            assert!(r.read_raw(p).is_err());
            assert_eq!(r.stat_raw(p).unwrap(), None);
        }
        assert!(r.remove_raw(b"link/victim").is_err());
        assert!(r.read_raw(b"link/victim").is_err());
        assert!(r.create_dir_all_raw(b"link/newdir").is_err());
        // Nothing was created outside the root and the victim is intact.
        let names: Vec<_> = std::fs::read_dir(outside.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("victim")]);
        assert_eq!(
            std::fs::read(outside.path().join("victim")).unwrap(),
            b"keep"
        );

        // A symlink at the final component is replaced, not followed.
        std::os::unix::fs::symlink(outside.path().join("victim"), dir.path().join("leaf")).unwrap();
        r.write_file_atomic_raw(b"leaf", b"new", Mode::File)
            .unwrap();
        assert_eq!(
            std::fs::read(outside.path().join("victim")).unwrap(),
            b"keep"
        );
        assert!(
            dir.path()
                .join("leaf")
                .symlink_metadata()
                .unwrap()
                .is_file()
        );
        // A regular file in a parent position is refused too.
        assert!(
            r.write_file_atomic_raw(b"leaf/x", b"y", Mode::File)
                .is_err()
        );
    }

    #[test]
    fn dir_at_target_is_refused() {
        let (dir, r) = root();
        std::fs::create_dir_all(dir.path().join("d/sub")).unwrap();
        let err = r.write_file_atomic_raw(b"d", b"x", Mode::File).unwrap_err();
        assert_eq!(err.subcode(), Some(subcode::FILE_DIRECTORY));
        assert!(r.remove_raw(b"d").is_err());
        assert!(r.write_file_atomic_raw(b"t", b"x", Mode::Tree).is_err());
        assert!(r.write_file_atomic_raw(b"", b"x", Mode::File).is_err());
        assert!(r.write_file_atomic_raw(b"../x", b"x", Mode::File).is_err());
        assert!(
            r.write_file_atomic_raw(b".scion/x", b"x", Mode::File)
                .is_err()
        );
        assert!(r.symlink_raw(b"s", b"").is_err());
        // Only the pre-existing directory remains.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("d")]);
    }

    #[test]
    fn case_probe_is_stable_and_clean() {
        let (dir, r) = root();
        let a = r.is_case_insensitive();
        assert_eq!(a, r.is_case_insensitive());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn write_atomic_replaces() {
        let dir = tempfile::tempdir().unwrap();
        write_atomic(dir.path(), "f", b"one", 0o600, true).unwrap();
        write_atomic(dir.path(), "f", b"two", 0o644, false).unwrap();
        assert_eq!(std::fs::read(dir.path().join("f")).unwrap(), b"two");
        let md = std::fs::metadata(dir.path().join("f")).unwrap();
        assert_eq!(md.permissions().mode() & 0o777, 0o644);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
