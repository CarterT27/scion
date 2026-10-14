//! The stat cache `.scion/index` (SPEC 11.1, 11.5, 18.7; PLAN 6.15; P11).
//!
//! Binary layout (all integers big-endian), version 1:
//!
//! ```text
//! magic "SCIONIDX" | version u32 | written_at_ns i128 | count u64
//! count x ( path_len u32 | path | size u64 | mtime_ns i128 | ctime_ns i128 | inode u64
//!           | st_mode u32 | id [34] | mode u8 | flags u8 (bit 0: redacted) )
//! blake3(all preceding bytes) [32]
//! ```
//!
//! `written_at_ns` is the filesystem timestamp of the freshly created temporary index file, so
//! racy detection compares file mtimes with a time from the same clock and granularity
//! (SPEC 11.5). The file is replaced atomically (temp file plus rename). A missing, truncated,
//! corrupt or unknown-version index loads as empty: it is a cache, and status then re-hashes
//! (unmodified placeholders are still recognised by their bytes).

use crate::fs_safe::io_error;
use scion_core::{Mode, ObjectId, Result};
use scion_object::RepoPath;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

/// File name of the index under `.scion/` (SPEC 11.1).
pub const INDEX_FILE: &str = "index";
/// Current binary format version.
pub const INDEX_VERSION: u32 = 1;
const MAGIC: &[u8; 8] = b"SCIONIDX";
const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

/// `(size, mtime_ns, ctime_ns, inode, mode)` of a file (SPEC 11.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatTuple {
    /// Size in bytes.
    pub size: u64,
    /// Modification time (ns).
    pub mtime_ns: i128,
    /// Change time (ns).
    pub ctime_ns: i128,
    /// Inode number.
    pub inode: u64,
    /// Unix mode bits.
    pub mode: u32,
}

impl StatTuple {
    /// A regular file.
    pub fn is_regular(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }

    /// A directory.
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }

    /// A symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.mode & S_IFMT == S_IFLNK
    }

    /// A regular file with the owner executable bit (git semantics, SPEC 4.3.2).
    pub fn is_executable(&self) -> bool {
        self.is_regular() && self.mode & 0o100 != 0
    }

    /// The tree entry mode this file would have (SPEC 4.3.2); `None` for directories and
    /// special files.
    pub fn entry_mode(&self) -> Option<Mode> {
        if self.is_symlink() {
            Some(Mode::Symlink)
        } else if self.is_executable() {
            Some(Mode::Executable)
        } else if self.is_regular() {
            Some(Mode::File)
        } else {
            None
        }
    }
}

/// One index entry (SPEC 11.5, 13.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// Stat tuple at the last verification.
    pub stat: StatTuple,
    /// Blob id of the content.
    pub id: ObjectId,
    /// Entry mode.
    pub mode: Mode,
    /// True when the file is a redaction placeholder.
    pub redacted: bool,
}

/// Versioned binary stat cache, saved atomically (SPEC 11.5, PLAN 6.15).
pub struct StatCache {
    /// Entries by path.
    pub entries: BTreeMap<RepoPath, IndexEntry>,
    /// When the index was written (ns), for racy detection.
    pub written_at_ns: i128,
}

impl Default for StatCache {
    fn default() -> Self {
        StatCache {
            entries: BTreeMap::new(),
            written_at_ns: 0,
        }
    }
}

impl Clone for StatCache {
    fn clone(&self) -> Self {
        StatCache {
            entries: self.entries.clone(),
            written_at_ns: self.written_at_ns,
        }
    }
}

impl std::fmt::Debug for StatCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatCache")
            .field("entries", &self.entries.len())
            .field("written_at_ns", &self.written_at_ns)
            .finish()
    }
}

impl StatCache {
    /// Load from `<scion_dir>/index` (empty when absent, truncated, corrupt or of an unknown
    /// version; SPEC 11.5).
    pub fn load(scion_dir: &Path) -> Result<Self> {
        let path = scion_dir.join(INDEX_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(io_error("read .scion/index", e)),
        };
        match decode(&bytes) {
            Ok(c) => Ok(c),
            Err(why) => {
                tracing::warn!(
                    reason = why,
                    "ignoring unusable .scion/index; status will re-hash"
                );
                Ok(Self::default())
            }
        }
    }

    /// Save atomically (temp file plus rename in `scion_dir`). `written_at_ns` becomes the
    /// filesystem timestamp of the new file's creation (SPEC 11.5 racy detection).
    pub fn save(&mut self, scion_dir: &Path) -> Result<()> {
        let mut tmp = tempfile::Builder::new()
            .prefix(".index.tmp-")
            .tempfile_in(scion_dir)
            .map_err(|e| io_error("create temporary index", e))?;
        let md = tmp
            .as_file()
            .metadata()
            .map_err(|e| io_error("stat temporary index", e))?;
        let written_at = {
            use std::os::unix::fs::MetadataExt;
            i128::from(md.mtime()) * 1_000_000_000 + i128::from(md.mtime_nsec())
        };
        let bytes = encode(&self.entries, written_at);
        tmp.as_file_mut()
            .write_all(&bytes)
            .map_err(|e| io_error("write temporary index", e))?;
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o644))
                .map_err(|e| io_error("set index mode", e))?;
        }
        tmp.persist(scion_dir.join(INDEX_FILE))
            .map_err(|e| io_error("rename temporary index", e.error))?;
        self.written_at_ns = written_at;
        Ok(())
    }

    /// Racy when `mtime >= written_at` (SPEC 11.5, 18.7): such entries are re-hashed.
    pub fn is_racy(&self, e: &IndexEntry) -> bool {
        e.stat.mtime_ns >= self.written_at_ns
    }
}

/// Serialize entries in the version-1 layout.
pub(crate) fn encode(entries: &BTreeMap<RepoPath, IndexEntry>, written_at_ns: i128) -> Vec<u8> {
    let mut out = Vec::with_capacity(48 + entries.len() * 128);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&INDEX_VERSION.to_be_bytes());
    out.extend_from_slice(&written_at_ns.to_be_bytes());
    out.extend_from_slice(&(entries.len() as u64).to_be_bytes());
    for (p, e) in entries {
        let path = p.as_bytes();
        out.extend_from_slice(&(path.len() as u32).to_be_bytes());
        out.extend_from_slice(path);
        out.extend_from_slice(&e.stat.size.to_be_bytes());
        out.extend_from_slice(&e.stat.mtime_ns.to_be_bytes());
        out.extend_from_slice(&e.stat.ctime_ns.to_be_bytes());
        out.extend_from_slice(&e.stat.inode.to_be_bytes());
        out.extend_from_slice(&e.stat.mode.to_be_bytes());
        out.extend_from_slice(e.id.as_bytes());
        out.push(e.mode as u8);
        out.push(u8::from(e.redacted));
    }
    let sum = blake3::hash(&out);
    out.extend_from_slice(sum.as_bytes());
    out
}

struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> std::result::Result<&'a [u8], &'static str> {
        if self.b.len() < n {
            return Err("truncated");
        }
        let (head, tail) = self.b.split_at(n);
        self.b = tail;
        Ok(head)
    }
    fn arr<const N: usize>(&mut self) -> std::result::Result<[u8; N], &'static str> {
        let s = self.take(N)?;
        let mut a = [0u8; N];
        a.copy_from_slice(s);
        Ok(a)
    }
    fn u8(&mut self) -> std::result::Result<u8, &'static str> {
        Ok(self.arr::<1>()?[0])
    }
    fn u32(&mut self) -> std::result::Result<u32, &'static str> {
        Ok(u32::from_be_bytes(self.arr()?))
    }
    fn u64(&mut self) -> std::result::Result<u64, &'static str> {
        Ok(u64::from_be_bytes(self.arr()?))
    }
    fn i128(&mut self) -> std::result::Result<i128, &'static str> {
        Ok(i128::from_be_bytes(self.arr()?))
    }
}

/// Parse the version-1 layout; the error is a short reason for the log.
pub(crate) fn decode(bytes: &[u8]) -> std::result::Result<StatCache, &'static str> {
    if bytes.len() < MAGIC.len() + 4 + 16 + 8 + 32 {
        return Err("truncated");
    }
    let (body, sum) = bytes.split_at(bytes.len() - 32);
    if &bytes[..8] != MAGIC {
        return Err("bad magic");
    }
    let mut r = Reader { b: &body[8..] };
    if r.u32()? != INDEX_VERSION {
        return Err("unknown version");
    }
    if blake3::hash(body).as_bytes() != sum {
        return Err("checksum mismatch");
    }
    let written_at_ns = r.i128()?;
    let count = r.u64()?;
    let mut entries = BTreeMap::new();
    for _ in 0..count {
        let len = r.u32()? as usize;
        let path = RepoPath::parse(r.take(len)?).map_err(|_| "invalid path")?;
        let stat = StatTuple {
            size: r.u64()?,
            mtime_ns: r.i128()?,
            ctime_ns: r.i128()?,
            inode: r.u64()?,
            mode: r.u32()?,
        };
        let id = ObjectId::from_bytes(r.take(34)?).map_err(|_| "invalid object id")?;
        let mode = Mode::from_u8(r.u8()?).map_err(|_| "invalid mode")?;
        let flags = r.u8()?;
        if flags & !1 != 0 {
            return Err("unknown flags");
        }
        entries.insert(
            path,
            IndexEntry {
                stat,
                id,
                mode,
                redacted: flags & 1 == 1,
            },
        );
    }
    if !r.b.is_empty() {
        return Err("trailing bytes");
    }
    Ok(StatCache {
        entries,
        written_at_ns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_round_trip_and_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let c = StatCache::load(dir.path()).unwrap();
        assert!(c.entries.is_empty());
        assert_eq!(c.written_at_ns, 0);

        let mut c = StatCache::default();
        c.save(dir.path()).unwrap();
        assert!(c.written_at_ns > 0);
        let back = StatCache::load(dir.path()).unwrap();
        assert_eq!(back.written_at_ns, c.written_at_ns);
        assert!(back.entries.is_empty());

        // Every truncation and every single-byte corruption loads as empty, never panics.
        let good = std::fs::read(dir.path().join(INDEX_FILE)).unwrap();
        for n in 0..good.len() {
            assert!(decode(&good[..n]).is_err());
            let mut bad = good.clone();
            bad[n] ^= 0x41;
            assert!(decode(&bad).is_err());
        }
        std::fs::write(dir.path().join(INDEX_FILE), b"garbage").unwrap();
        let c = StatCache::load(dir.path()).unwrap();
        assert!(c.entries.is_empty() && c.written_at_ns == 0);
    }

    #[test]
    fn racy_predicate() {
        let e = IndexEntry {
            stat: StatTuple {
                size: 1,
                mtime_ns: 100,
                ctime_ns: 100,
                inode: 1,
                mode: 0o100644,
            },
            id: ObjectId::hash_blob(b"x"),
            mode: Mode::File,
            redacted: false,
        };
        let mut c = StatCache::default();
        assert!(c.is_racy(&e), "never-written index: everything is racy");
        c.written_at_ns = 100;
        assert!(c.is_racy(&e), "mtime == written_at is racy");
        c.written_at_ns = 101;
        assert!(!c.is_racy(&e));
    }

    #[test]
    fn stat_tuple_kinds() {
        let t = |mode| StatTuple {
            size: 0,
            mtime_ns: 0,
            ctime_ns: 0,
            inode: 0,
            mode,
        };
        assert_eq!(t(0o100644).entry_mode(), Some(Mode::File));
        assert_eq!(t(0o100744).entry_mode(), Some(Mode::Executable));
        assert_eq!(t(0o100654).entry_mode(), Some(Mode::File));
        assert_eq!(t(0o120777).entry_mode(), Some(Mode::Symlink));
        assert_eq!(t(0o040755).entry_mode(), None);
        assert!(t(0o040755).is_dir());
        assert_eq!(t(0o010644).entry_mode(), None);
    }
}
