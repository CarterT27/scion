//! Verified content-addressed disk cache (SPEC 10.2, 16.1 classes 6 and 7; P10).
//!
//! On-disk layout: `<dir>/objects/<kind_letter>/<hex[0:2]>/<hex>`. Each entry file holds a
//! 1-byte header ([`HEADER_RAW`] or [`HEADER_ZSTD`]) followed by the payload exactly as it was
//! received from upstream (raw content or a zstd frame of the content). Temp files live in the
//! same directory as their final entry and carry the [`TEMP_PREFIX`] name prefix, so a crash
//! between the temp write and the rename never leaves a visible entry, and `open` deletes them.
//!
//! Every object id is `BLAKE3(content)` regardless of kind (SPEC 4.2), so an entry is verified
//! by hashing the (decompressed) content and comparing it with the id digest.

use bytes::Bytes;
use parking_lot::Mutex;
use scion_core::{ErrorCode, ObjectId, Result, ScionError};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Header byte of an entry holding raw content.
const HEADER_RAW: u8 = 0x00;
/// Header byte of an entry holding a zstd frame of the content.
const HEADER_ZSTD: u8 = 0x01;
/// Name prefix of in-progress insert files. Entry names are lowercase hex, so they never
/// collide with this prefix.
const TEMP_PREFIX: &str = ".tmp-";
/// Kind letters used as top-level directories (`Kind::letter`).
const KIND_LETTERS: [u8; 5] = *b"btcmx";

/// A payload as received from upstream (SPEC 10.2: cached "as received").
#[derive(Clone, Debug)]
pub enum Payload {
    /// Raw content.
    Raw(Bytes),
    /// A zstd frame of the content.
    Zstd(Bytes),
}

/// Index key: kind letter plus digest. The file content is determined by the digest alone,
/// so derived kinds that share the `x` directory also share an entry safely.
type Key = (u8, [u8; 32]);

/// Per-entry bookkeeping.
#[derive(Clone, Copy, Debug)]
struct Entry {
    /// File length in bytes (header included).
    size: u64,
    /// Logical access clock value of the last insert or read (approximate LRU).
    last_access: u64,
    /// Distinguishes successive incarnations of an entry for the same key.
    generation: u64,
}

/// In-memory size and access index, rebuilt by scanning on `open`.
#[derive(Debug, Default)]
struct Index {
    entries: HashMap<Key, Entry>,
    usage: u64,
    tick: u64,
    next_generation: u64,
}

impl Index {
    fn next_tick(&mut self) -> u64 {
        self.tick = self.tick.wrapping_add(1);
        self.tick
    }

    fn add(&mut self, key: Key, size: u64) {
        let last_access = self.next_tick();
        self.next_generation = self.next_generation.wrapping_add(1);
        let entry = Entry {
            size,
            last_access,
            generation: self.next_generation,
        };
        if let Some(old) = self.entries.insert(key, entry) {
            self.usage = self.usage.saturating_sub(old.size);
        }
        self.usage = self.usage.saturating_add(size);
    }

    fn remove(&mut self, key: &Key) -> Option<Entry> {
        let e = self.entries.remove(key)?;
        self.usage = self.usage.saturating_sub(e.size);
        Some(e)
    }
}

/// Where an insert stops; anything but `Complete` exists to simulate crashes in tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InsertStop {
    /// Run the whole insert.
    Complete,
    /// Leave the fsynced temp file behind and return before the rename (a crash).
    #[cfg_attr(not(test), allow(dead_code))]
    BeforeRename,
}

/// `<dir>/objects/<kind_letter>/<hex[0:2]>/<hex>` with approximate-LRU eviction to the low
/// watermark (SPEC 10.2).
///
/// All methods are synchronous filesystem operations; async callers should run them on a
/// blocking thread.
pub struct DiskCache {
    objects: PathBuf,
    max_bytes: u64,
    low_watermark: f64,
    verify_on_read: bool,
    index: Mutex<Index>,
}

impl std::fmt::Debug for DiskCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskCache")
            .field("objects", &self.objects)
            .field("max_bytes", &self.max_bytes)
            .field("low_watermark", &self.low_watermark)
            .field("verify_on_read", &self.verify_on_read)
            .field("usage", &self.usage())
            .finish()
    }
}

fn io_err(ctx: &str, path: &Path, e: io::Error) -> ScionError {
    ScionError::new(
        ErrorCode::Unavailable,
        format!("disk cache: {ctx} {}: {e}", path.display()),
    )
}

fn key_of(id: &ObjectId) -> Key {
    // Kind letters are ASCII.
    (id.kind().letter() as u8, *id.digest())
}

fn is_hex64(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn parse_digest(name: &str) -> Option<[u8; 32]> {
    if !is_hex64(name) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in name.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

fn hex_of(digest: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(64);
    for b in digest {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// `fsync(2)` without macOS `F_FULLFSYNC` (which `File::sync_all` uses and which is far slower).
fn fsync_file(f: &fs::File, path: &Path) -> Result<()> {
    rustix::fs::fsync(f).map_err(|e| io_err("fsync", path, e.into()))
}

/// Hash `content` and compare with the id digest.
fn digest_matches(id: &ObjectId, content: &[u8]) -> bool {
    blake3::hash(content).as_bytes() == id.digest()
}

/// Stream-decompress a zstd frame, hashing the output; `None` when the frame is malformed.
fn zstd_digest(frame: &[u8]) -> Option<[u8; 32]> {
    let mut decoder = zstd::stream::read::Decoder::new(frame).ok()?;
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut decoder, &mut hasher).ok()?;
    Some(*hasher.finalize().as_bytes())
}

/// True when the payload's content hashes to the id digest.
fn payload_matches(id: &ObjectId, p: &Payload) -> bool {
    match p {
        Payload::Raw(b) => digest_matches(id, b),
        Payload::Zstd(frame) => zstd_digest(frame).is_some_and(|d| &d == id.digest()),
    }
}

/// Split a stored file into its payload; `None` for an unknown header or an empty file.
fn decode_file(data: Bytes) -> Option<Payload> {
    let header = *data.first()?;
    let body = data.slice(1..);
    match header {
        HEADER_RAW => Some(Payload::Raw(body)),
        HEADER_ZSTD => Some(Payload::Zstd(body)),
        _ => None,
    }
}

impl DiskCache {
    /// Open (creating) the cache: deletes leftover temp files and rebuilds the size and access
    /// index by scanning (SPEC 10.2 startup, 16.1 class 6). `low_watermark` must be within
    /// `0.0..=1.0` (INVALID_ARGUMENT otherwise). When the scanned usage already exceeds
    /// `max_bytes`, the oldest entries (by file modification time) are evicted.
    pub fn open(
        dir: &Path,
        max_bytes: u64,
        low_watermark: f64,
        verify_on_read: bool,
    ) -> Result<Self> {
        if !(0.0..=1.0).contains(&low_watermark) {
            return Err(ScionError::new(
                ErrorCode::InvalidArgument,
                format!("disk cache: low_watermark {low_watermark} is outside 0.0..=1.0"),
            ));
        }
        let objects = dir.join("objects");
        fs::create_dir_all(&objects).map_err(|e| io_err("create", &objects, e))?;
        let cache = DiskCache {
            objects,
            max_bytes,
            low_watermark,
            verify_on_read,
            index: Mutex::new(Index::default()),
        };
        cache.rebuild()?;
        Ok(cache)
    }

    /// Scan the tree: delete temp files and empty entries, index everything else.
    fn rebuild(&self) -> Result<()> {
        let mut found: Vec<(SystemTime, Key, u64)> = Vec::new();
        let mut temp_removed = 0usize;
        remove_temp_files_in(&self.objects, &mut temp_removed)?;
        for &letter in &KIND_LETTERS {
            let letter_dir = self.objects.join((letter as char).to_string());
            if !letter_dir.is_dir() {
                continue;
            }
            remove_temp_files_in(&letter_dir, &mut temp_removed)?;
            let shards = fs::read_dir(&letter_dir).map_err(|e| io_err("scan", &letter_dir, e))?;
            for shard in shards {
                let shard = shard.map_err(|e| io_err("scan", &letter_dir, e))?;
                let shard_path = shard.path();
                if !shard_path.is_dir() {
                    continue;
                }
                let shard_name = shard.file_name().to_string_lossy().into_owned();
                let files =
                    fs::read_dir(&shard_path).map_err(|e| io_err("scan", &shard_path, e))?;
                for file in files {
                    let file = file.map_err(|e| io_err("scan", &shard_path, e))?;
                    let name = file.file_name().to_string_lossy().into_owned();
                    let path = file.path();
                    if name.starts_with(TEMP_PREFIX) {
                        remove_if_exists(&path)?;
                        temp_removed += 1;
                        continue;
                    }
                    let Some(digest) = parse_digest(&name).filter(|_| name[..2] == shard_name)
                    else {
                        tracing::warn!(path = %path.display(), "disk cache: ignoring unexpected file");
                        continue;
                    };
                    let meta = file.metadata().map_err(|e| io_err("stat", &path, e))?;
                    if !meta.is_file() {
                        continue;
                    }
                    if meta.len() == 0 {
                        remove_if_exists(&path)?;
                        continue;
                    }
                    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    found.push((mtime, (letter, digest), meta.len()));
                }
            }
        }
        // Oldest modification time gets the oldest access tick.
        found.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let mut idx = self.index.lock();
        *idx = Index::default();
        for (_, key, size) in found {
            idx.add(key, size);
        }
        let entries = idx.entries.len();
        let usage = idx.usage;
        if idx.usage > self.max_bytes {
            self.evict_locked(&mut idx, 0);
        }
        drop(idx);
        tracing::info!(entries, usage, temp_removed, "disk cache opened");
        Ok(())
    }

    fn shard_dir(&self, key: &Key) -> PathBuf {
        let hex = hex_of(&key.1);
        self.objects
            .join((key.0 as char).to_string())
            .join(&hex[..2])
    }

    fn entry_path(&self, key: &Key) -> PathBuf {
        let hex = hex_of(&key.1);
        self.objects
            .join((key.0 as char).to_string())
            .join(&hex[..2])
            .join(hex)
    }

    /// Bytes allowed after an eviction pass.
    fn low_target(&self) -> u64 {
        // Truncation toward zero keeps the target at or below the watermark.
        (self.max_bytes as f64 * self.low_watermark) as u64
    }

    /// Evict least-recently-accessed entries so that `usage + incoming` fits the low watermark
    /// (and never exceeds `max_bytes`). Called with the index locked.
    fn evict_locked(&self, idx: &mut Index, incoming: u64) {
        let target = self.low_target();
        if idx.usage.saturating_add(incoming) <= target {
            return;
        }
        let mut order: Vec<(u64, Key)> = idx
            .entries
            .iter()
            .map(|(k, e)| (e.last_access, *k))
            .collect();
        order.sort_unstable();
        let mut evicted = 0usize;
        for (_, key) in order {
            if idx.usage.saturating_add(incoming) <= target {
                break;
            }
            if idx.remove(&key).is_some() {
                let path = self.entry_path(&key);
                if let Err(e) = remove_if_exists(&path) {
                    tracing::warn!(error = %e, "disk cache: eviction could not delete a file");
                }
                evicted += 1;
            }
        }
        tracing::debug!(evicted, usage = idx.usage, "disk cache eviction");
    }

    /// Raw content, verified when `verify_on_read`; a mismatch (or an undecodable entry)
    /// deletes the entry, raises an integrity alert and returns `Ok(None)` so the caller
    /// re-fetches upstream (SPEC 10.2, 16.1 class 7).
    pub fn get(&self, id: &ObjectId) -> Result<Option<Bytes>> {
        let key = key_of(id);
        let Some((generation, payload)) = self.read_payload(&key)? else {
            return Ok(None);
        };
        let content = match payload {
            Payload::Raw(b) => Some(b),
            Payload::Zstd(frame) => zstd::stream::decode_all(&frame[..]).ok().map(Bytes::from),
        };
        match content {
            Some(c) if !self.verify_on_read || digest_matches(id, &c) => Ok(Some(c)),
            _ => {
                self.discard_corrupt(id, &key, generation);
                Ok(None)
            }
        }
    }

    /// The payload as stored, verified when `verify_on_read` (a mismatch deletes the entry
    /// and returns `Ok(None)`, as for [`DiskCache::get`]).
    pub fn get_payload(&self, id: &ObjectId) -> Result<Option<Payload>> {
        let key = key_of(id);
        let Some((generation, payload)) = self.read_payload(&key)? else {
            return Ok(None);
        };
        if self.verify_on_read && !payload_matches(id, &payload) {
            self.discard_corrupt(id, &key, generation);
            return Ok(None);
        }
        Ok(Some(payload))
    }

    /// Look up and read an entry, bumping its access time. A missing file drops the index
    /// entry; a file with a bad header is discarded as corrupt.
    fn read_payload(&self, key: &Key) -> Result<Option<(u64, Payload)>> {
        let generation = {
            let mut idx = self.index.lock();
            let tick = idx.next_tick();
            match idx.entries.get_mut(key) {
                Some(e) => {
                    e.last_access = tick;
                    e.generation
                }
                None => return Ok(None),
            }
        };
        let path = self.entry_path(key);
        let data = match fs::read(&path) {
            Ok(d) => Bytes::from(d),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.forget_if_generation(key, generation);
                return Ok(None);
            }
            Err(e) => return Err(io_err("read", &path, e)),
        };
        match decode_file(data) {
            Some(p) => Ok(Some((generation, p))),
            None => {
                let mut idx = self.index.lock();
                if idx.entries.get(key).map(|e| e.generation) == Some(generation) {
                    idx.remove(key);
                    drop(idx);
                    let _ = remove_if_exists(&path);
                }
                tracing::error!(
                    event = "integrity_alert",
                    tier = "daemon",
                    path = %path.display(),
                    "disk cache entry has an invalid header; deleted"
                );
                Ok(None)
            }
        }
    }

    /// Drop the index entry for `key` when it is still the incarnation we looked at.
    fn forget_if_generation(&self, key: &Key, generation: u64) {
        let mut idx = self.index.lock();
        if idx.entries.get(key).map(|e| e.generation) == Some(generation) {
            idx.remove(key);
        }
    }

    /// Delete a corrupt entry (unless it was replaced meanwhile) and raise the SPEC 16.1
    /// class 7 integrity alert (PLAN 6.10).
    fn discard_corrupt(&self, id: &ObjectId, key: &Key, generation: u64) {
        let mut idx = self.index.lock();
        if idx.entries.get(key).map(|e| e.generation) == Some(generation) {
            idx.remove(key);
            let path = self.entry_path(key);
            if let Err(e) = remove_if_exists(&path) {
                tracing::warn!(error = %e, "disk cache: could not delete a corrupt entry");
            }
        }
        drop(idx);
        tracing::error!(
            event = "integrity_alert",
            tier = "daemon",
            object = %id,
            "disk cache entry does not match its identifier; deleted"
        );
    }

    /// Insert: verify the digest of the decompressed content against the id (INTEGRITY and
    /// nothing cached on mismatch), write a temp file in the entry's directory, fsync, then
    /// rename it into place (SPEC 10.2). If the entry would push usage over `max_bytes`,
    /// least-recently-accessed entries are evicted first down to the low watermark, so usage
    /// never exceeds `max_bytes`. A payload larger than `max_bytes` is verified but not cached.
    /// Inserting an id that is already cached is a no-op.
    pub fn insert(&self, id: &ObjectId, p: Payload) -> Result<()> {
        self.insert_inner(id, p, InsertStop::Complete)
    }

    fn insert_inner(&self, id: &ObjectId, p: Payload, stop: InsertStop) -> Result<()> {
        if !payload_matches(id, &p) {
            tracing::warn!(object = %id, "disk cache: rejected an insert whose digest does not match");
            return Err(ScionError::integrity(id));
        }
        let (header, body) = match &p {
            Payload::Raw(b) => (HEADER_RAW, b),
            Payload::Zstd(b) => (HEADER_ZSTD, b),
        };
        let size = (body.len() as u64).saturating_add(1);
        if size > self.max_bytes {
            tracing::debug!(object = %id, size, "disk cache: payload exceeds max_bytes; not cached");
            return Ok(());
        }
        let key = key_of(id);
        if self.index.lock().entries.contains_key(&key) {
            return Ok(());
        }
        let shard = self.shard_dir(&key);
        fs::create_dir_all(&shard).map_err(|e| io_err("create", &shard, e))?;
        let mut tmp = tempfile::Builder::new()
            .prefix(TEMP_PREFIX)
            .tempfile_in(&shard)
            .map_err(|e| io_err("create temp in", &shard, e))?;
        tmp.write_all(&[header])
            .and_then(|()| tmp.write_all(body))
            .and_then(|()| tmp.flush())
            .map_err(|e| io_err("write", tmp.path(), e))?;
        fsync_file(tmp.as_file(), tmp.path())?;
        if stop == InsertStop::BeforeRename {
            // Simulated crash: the fsynced temp file stays behind, nothing is renamed.
            let (_file, path) = tmp.keep().map_err(|e| io_err("keep", &shard, e.error))?;
            return Err(ScionError::new(
                ErrorCode::Unavailable,
                format!(
                    "disk cache: simulated crash before rename of {}",
                    path.display()
                ),
            ));
        }
        let path = self.entry_path(&key);
        {
            let mut idx = self.index.lock();
            if idx.entries.contains_key(&key) {
                // A concurrent insert won; the temp file is deleted on drop.
                return Ok(());
            }
            if idx.usage.saturating_add(size) > self.max_bytes {
                self.evict_locked(&mut idx, size);
            }
            tmp.persist(&path)
                .map_err(|e| io_err("rename into", &path, e.error))?;
            idx.add(key, size);
        }
        // Make the rename durable. The entry is already complete and verified, so a failure
        // here is only logged.
        match fs::File::open(&shard) {
            Ok(d) => {
                if let Err(e) = fsync_file(&d, &shard) {
                    tracing::warn!(error = %e, "disk cache: directory fsync failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "disk cache: directory open for fsync failed"),
        }
        Ok(())
    }

    /// Remove an entry (used on redaction, SPEC 13.3). Removing an absent id is a no-op.
    pub fn remove(&self, id: &ObjectId) -> Result<()> {
        let key = key_of(id);
        let mut idx = self.index.lock();
        idx.remove(&key);
        remove_if_exists(&self.entry_path(&key))
    }

    /// True when cached.
    pub fn contains(&self, id: &ObjectId) -> bool {
        self.index.lock().entries.contains_key(&key_of(id))
    }

    /// Exact bytes used by cached entries (file lengths, 1-byte headers included).
    pub fn usage(&self) -> u64 {
        self.index.lock().usage
    }

    /// Number of cached entries (SPEC 10.2; additive helper for stats and tests).
    pub fn len(&self) -> usize {
        self.index.lock().entries.len()
    }

    /// True when nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The configured capacity (`max_cache_bytes`, SPEC 10.2).
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// The on-disk path of the entry for `id`, whether or not it is cached (SPEC 10.2
    /// layout; for diagnostics and tests).
    pub fn entry_path_of(&self, id: &ObjectId) -> PathBuf {
        self.entry_path(&key_of(id))
    }
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("delete", path, e)),
    }
}

/// Delete temp files directly inside `dir`.
fn remove_temp_files_in(dir: &Path, removed: &mut usize) -> Result<()> {
    let rd = fs::read_dir(dir).map_err(|e| io_err("scan", dir, e))?;
    for ent in rd {
        let ent = ent.map_err(|e| io_err("scan", dir, e))?;
        if ent.file_name().to_string_lossy().starts_with(TEMP_PREFIX) && ent.path().is_file() {
            remove_if_exists(&ent.path())?;
            *removed += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zstd_payload(raw: &[u8]) -> Payload {
        Payload::Zstd(Bytes::from(zstd::encode_all(raw, 3).unwrap()))
    }

    fn temp_files(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(TEMP_PREFIX)
                {
                    out.push(p);
                }
            }
        }
        out
    }

    #[test]
    fn crash_after_temp_write_before_rename_leaves_no_entry() {
        let dir = tempfile::tempdir().unwrap();
        let raw = b"crash me".to_vec();
        let id = ObjectId::hash_blob(&raw);
        {
            let c = DiskCache::open(dir.path(), 1 << 20, 0.9, true).unwrap();
            let err = c
                .insert_inner(&id, zstd_payload(&raw), InsertStop::BeforeRename)
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Unavailable);
            assert!(!c.contains(&id));
            assert_eq!(c.get(&id).unwrap(), None);
            assert_eq!(c.usage(), 0);
            assert!(!c.entry_path_of(&id).exists());
            assert_eq!(temp_files(dir.path()).len(), 1);
        }
        // Restart: the temp file is deleted and nothing is visible.
        let c = DiskCache::open(dir.path(), 1 << 20, 0.9, true).unwrap();
        assert!(temp_files(dir.path()).is_empty());
        assert!(!c.contains(&id));
        assert_eq!(c.get(&id).unwrap(), None);
        assert_eq!(c.usage(), 0);
        // A retried insert works.
        c.insert(&id, Payload::Raw(Bytes::from(raw.clone())))
            .unwrap();
        assert_eq!(c.get(&id).unwrap().unwrap(), Bytes::from(raw));
    }

    #[test]
    fn hex_roundtrip() {
        let d = [0xabu8; 32];
        let h = hex_of(&d);
        assert_eq!(h.len(), 64);
        assert_eq!(parse_digest(&h), Some(d));
        assert_eq!(parse_digest("zz"), None);
        assert_eq!(parse_digest(&h.to_uppercase()), None);
    }

    #[test]
    fn layout_matches_spec() {
        let dir = tempfile::tempdir().unwrap();
        let c = DiskCache::open(dir.path(), 1 << 20, 0.9, false).unwrap();
        let raw = b"layout".to_vec();
        let id = ObjectId::hash_blob(&raw);
        c.insert(&id, Payload::Raw(Bytes::from(raw.clone())))
            .unwrap();
        let hex = id.hex();
        let expected = dir
            .path()
            .join("objects")
            .join("b")
            .join(&hex[..2])
            .join(&hex);
        assert_eq!(c.entry_path_of(&id), expected);
        let data = fs::read(&expected).unwrap();
        assert_eq!(data[0], HEADER_RAW);
        assert_eq!(&data[1..], &raw[..]);
        assert_eq!(c.usage(), data.len() as u64);
    }

    #[test]
    fn rejects_bad_watermark() {
        let dir = tempfile::tempdir().unwrap();
        assert!(DiskCache::open(dir.path(), 10, 1.5, false).is_err());
        assert!(DiskCache::open(dir.path(), 10, f64::NAN, false).is_err());
    }
}
