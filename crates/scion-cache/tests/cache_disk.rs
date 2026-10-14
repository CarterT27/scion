//! SPEC 19.9 daemon cache, unit level: the verified disk cache.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test helpers

use bytes::Bytes;
use scion_cache::{DiskCache, Payload};
use scion_core::{ErrorCode, Kind, ObjectId};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

fn raw(b: &[u8]) -> Payload {
    Payload::Raw(Bytes::copy_from_slice(b))
}

fn zst(b: &[u8]) -> Payload {
    Payload::Zstd(Bytes::from(zstd::encode_all(b, 3).unwrap()))
}

fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn disk_bytes(dir: &Path) -> u64 {
    all_files(dir)
        .iter()
        .map(|p| fs::metadata(p).unwrap().len())
        .sum()
}

/// Tiny deterministic PRNG (xorshift64*).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn insert_and_get_roundtrip_all_kinds_and_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let c = DiskCache::open(dir.path(), 1 << 20, 0.9, true).unwrap();
    let content = b"some canonical bytes".to_vec();
    let ids = [
        ObjectId::hash_blob(&content),
        ObjectId::hash_encoded(Kind::Tree, &content),
        ObjectId::hash_encoded(Kind::Changeset, &content),
        ObjectId::hash_encoded(Kind::Meta, &content),
        ObjectId::hash_encoded(Kind::Derived(0x10), &content),
    ];
    for (i, id) in ids.iter().enumerate() {
        assert!(!c.contains(id));
        let p = if i % 2 == 0 {
            raw(&content)
        } else {
            zst(&content)
        };
        c.insert(id, p).unwrap();
        assert!(c.contains(id));
        assert_eq!(c.get(id).unwrap().unwrap(), Bytes::from(content.clone()));
        let hex = id.hex();
        let letter = id.kind().letter().to_string();
        assert!(
            dir.path()
                .join("objects")
                .join(letter)
                .join(&hex[..2])
                .join(&hex)
                .is_file()
        );
    }
    // Payload comes back exactly as received.
    match c.get_payload(&ids[1]).unwrap().unwrap() {
        Payload::Zstd(f) => assert_eq!(zstd::decode_all(&f[..]).unwrap(), content),
        Payload::Raw(_) => panic!("expected the zstd frame as received"),
    }
    match c.get_payload(&ids[0]).unwrap().unwrap() {
        Payload::Raw(b) => assert_eq!(b, Bytes::from(content.clone())),
        Payload::Zstd(_) => panic!("expected raw as received"),
    }
    assert_eq!(c.usage(), disk_bytes(dir.path()));
    // Re-inserting is a no-op.
    let before = c.usage();
    c.insert(&ids[0], zst(&content)).unwrap();
    assert_eq!(c.usage(), before);
}

#[test]
fn corrupt_insert_is_rejected_and_not_cached() {
    let dir = tempfile::tempdir().unwrap();
    let c = DiskCache::open(dir.path(), 1 << 20, 0.9, true).unwrap();
    let good = b"the real content".to_vec();
    let id = ObjectId::hash_blob(&good);
    let bad_cases = vec![
        raw(b"tampered content"),
        zst(b"tampered content"),
        Payload::Zstd(Bytes::from_static(b"not a zstd frame at all")),
        Payload::Raw(Bytes::new()),
    ];
    for p in bad_cases {
        let err = c.insert(&id, p).unwrap_err();
        assert_eq!(err.code, ErrorCode::Integrity);
        assert!(err.retryable());
        assert!(!c.contains(&id));
        assert_eq!(c.get(&id).unwrap(), None);
        assert_eq!(c.usage(), 0);
    }
    // Kind matters for the path only; a digest mismatch is caught for every kind.
    let tree_id = ObjectId::hash_encoded(Kind::Tree, b"tree bytes");
    assert_eq!(
        c.insert(&tree_id, raw(b"other bytes")).unwrap_err().code,
        ErrorCode::Integrity
    );
    // Nothing (not even a temp file) was left on disk.
    assert!(all_files(dir.path()).is_empty());
    // The genuine content is accepted afterwards.
    c.insert(&id, raw(&good)).unwrap();
    assert_eq!(c.get(&id).unwrap().unwrap(), Bytes::from(good));
}

fn flip_last_bit(path: &Path) {
    let mut data = fs::read(path).unwrap();
    let n = data.len() - 1;
    data[n] ^= 0x01;
    fs::write(path, data).unwrap();
}

#[test]
fn bit_flip_detected_on_read_deleted_and_refetched() {
    let dir = tempfile::tempdir().unwrap();
    let c = DiskCache::open(dir.path(), 1 << 20, 0.9, true).unwrap();
    let content: Vec<u8> = (0..4096u32).map(|i| (i * 7 % 251) as u8).collect();
    let id = ObjectId::hash_blob(&content);

    for payload in [raw(&content), zst(&content)] {
        c.insert(&id, payload.clone()).unwrap();
        let path = c.entry_path_of(&id);
        flip_last_bit(&path);
        assert_eq!(c.get(&id).unwrap(), None, "corruption must be detected");
        assert!(!c.contains(&id));
        assert!(!path.exists(), "corrupt entry is deleted");
        assert_eq!(c.usage(), 0);
        // Re-fetch from upstream and serve again.
        c.insert(&id, payload.clone()).unwrap();
        assert_eq!(c.get(&id).unwrap().unwrap(), Bytes::from(content.clone()));

        // The payload path verifies as well.
        flip_last_bit(&path);
        assert!(c.get_payload(&id).unwrap().is_none());
        assert!(!c.contains(&id));
        assert!(!path.exists());
        assert_eq!(c.usage(), 0);
    }

    // A clobbered header is treated as corruption too.
    c.insert(&id, raw(&content)).unwrap();
    let path = c.entry_path_of(&id);
    let mut data = fs::read(&path).unwrap();
    data[0] = 0x7f;
    fs::write(&path, data).unwrap();
    assert_eq!(c.get(&id).unwrap(), None);
    assert!(!path.exists());
}

#[test]
fn bit_flip_in_unverified_zstd_entry_never_serves_garbage_silently() {
    // Without verify_on_read a raw entry is served as stored (SPEC 10.2 makes read
    // verification optional); a broken zstd frame still cannot be decoded and is dropped.
    let dir = tempfile::tempdir().unwrap();
    let c = DiskCache::open(dir.path(), 1 << 20, 0.9, false).unwrap();
    let content = vec![42u8; 10_000];
    let id = ObjectId::hash_blob(&content);
    c.insert(&id, zst(&content)).unwrap();
    let path = c.entry_path_of(&id);
    let mut data = fs::read(&path).unwrap();
    data.truncate(data.len() / 2);
    fs::write(&path, data).unwrap();
    assert_eq!(c.get(&id).unwrap(), None);
    assert!(!c.contains(&id));
}

#[test]
fn temp_files_removed_on_open_and_index_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    let mut ids = Vec::new();
    {
        let c = DiskCache::open(dir.path(), 1 << 20, 0.9, true).unwrap();
        for i in 0..20u32 {
            let content = format!("object number {i}").into_bytes();
            let id = ObjectId::hash_blob(&content);
            let p = if i % 2 == 0 {
                raw(&content)
            } else {
                zst(&content)
            };
            c.insert(&id, p).unwrap();
            ids.push((id, content));
        }
    }
    // Simulate crashed inserts: temp files next to entries and at upper levels, with partial
    // and with complete content.
    let shard = ids[0].0.hex()[..2].to_owned();
    let shard_dir = dir.path().join("objects").join("b").join(&shard);
    fs::write(shard_dir.join(".tmp-crash1"), b"\x00partial").unwrap();
    fs::write(shard_dir.join(".tmp-crash2"), b"").unwrap();
    fs::write(dir.path().join("objects").join(".tmp-x"), b"junk").unwrap();
    fs::write(dir.path().join("objects").join("b").join(".tmp-y"), b"junk").unwrap();
    // An empty file with an entry name (crash between create and write on a broken
    // filesystem) is discarded too.
    let empty_entry = ObjectId::hash_blob(b"never written");
    let empty_path = dir
        .path()
        .join("objects")
        .join("b")
        .join(&empty_entry.hex()[..2])
        .join(empty_entry.hex());
    fs::create_dir_all(empty_path.parent().unwrap()).unwrap();
    fs::write(&empty_path, b"").unwrap();

    let c = DiskCache::open(dir.path(), 1 << 20, 0.9, true).unwrap();
    let leftovers: Vec<_> = all_files(dir.path())
        .into_iter()
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(!empty_path.exists());
    assert!(!c.contains(&empty_entry));
    assert_eq!(c.len(), ids.len());
    assert_eq!(c.usage(), disk_bytes(dir.path()));
    for (id, content) in &ids {
        assert!(c.contains(id));
        assert_eq!(c.get(id).unwrap().unwrap(), Bytes::from(content.clone()));
    }
}

#[test]
fn reopen_with_smaller_capacity_evicts() {
    let dir = tempfile::tempdir().unwrap();
    {
        let c = DiskCache::open(dir.path(), 1 << 20, 0.9, false).unwrap();
        for i in 0..50u32 {
            let content = vec![i as u8; 1000];
            c.insert(&ObjectId::hash_blob(&content), raw(&content))
                .unwrap();
        }
        assert_eq!(c.usage(), 50 * 1001);
    }
    let c = DiskCache::open(dir.path(), 10_010, 0.5, false).unwrap();
    assert!(c.usage() <= 5005);
    assert_eq!(c.usage(), disk_bytes(dir.path()));
}

#[test]
fn remove_deletes_entry() {
    let dir = tempfile::tempdir().unwrap();
    let c = DiskCache::open(dir.path(), 1 << 20, 0.9, true).unwrap();
    let content = b"redact me".to_vec();
    let id = ObjectId::hash_blob(&content);
    c.insert(&id, raw(&content)).unwrap();
    c.remove(&id).unwrap();
    assert!(!c.contains(&id));
    assert_eq!(c.get(&id).unwrap(), None);
    assert!(!c.entry_path_of(&id).exists());
    assert_eq!(c.usage(), 0);
    // Removing again is fine.
    c.remove(&id).unwrap();
}

#[test]
fn oversized_payload_is_not_cached() {
    let dir = tempfile::tempdir().unwrap();
    let c = DiskCache::open(dir.path(), 100, 0.9, true).unwrap();
    let content = vec![1u8; 200];
    let id = ObjectId::hash_blob(&content);
    c.insert(&id, raw(&content)).unwrap();
    assert!(!c.contains(&id));
    assert_eq!(c.usage(), 0);
    // Still verified.
    assert_eq!(
        c.insert(&id, raw(&[2u8; 200])).unwrap_err().code,
        ErrorCode::Integrity
    );
}

#[test]
fn eviction_is_approximately_lru() {
    let dir = tempfile::tempdir().unwrap();
    // Room for 10 entries of 101 bytes; evict down to 5.
    let c = DiskCache::open(dir.path(), 1010, 0.5, false).unwrap();
    let mk = |i: u32| {
        let content = vec![i as u8; 100];
        (ObjectId::hash_blob(&content), content)
    };
    for i in 0..10 {
        let (id, content) = mk(i);
        c.insert(&id, raw(&content)).unwrap();
    }
    assert_eq!(c.usage(), 1010);
    // Touch the oldest two so they become most recent.
    assert!(c.get(&mk(0).0).unwrap().is_some());
    assert!(c.get_payload(&mk(1).0).unwrap().is_some());
    let (id10, content10) = mk(10);
    c.insert(&id10, raw(&content10)).unwrap();
    assert!(c.usage() <= 505);
    assert!(c.contains(&id10));
    assert!(c.contains(&mk(0).0));
    assert!(c.contains(&mk(1).0));
    for i in 2..8 {
        assert!(!c.contains(&mk(i).0), "entry {i} should have been evicted");
    }
    assert_eq!(c.usage(), disk_bytes(dir.path()));
}

/// Read-through helper modelling the daemon: a miss (including an evicted or corrupt entry)
/// is re-fetched from "upstream" transparently.
fn read_through(
    c: &DiskCache,
    upstream: &HashMap<ObjectId, Vec<u8>>,
    id: &ObjectId,
    fetches: &mut u64,
) -> Bytes {
    if let Some(b) = c.get(id).unwrap() {
        return b;
    }
    *fetches += 1;
    let content = &upstream[id];
    c.insert(id, raw(content)).unwrap();
    Bytes::from(content.clone())
}

fn randomized_workload(seed: u64, ops: usize) {
    let dir = tempfile::tempdir().unwrap();
    let max = 64 * 1024;
    let c = DiskCache::open(dir.path(), max, 0.75, seed.is_multiple_of(2)).unwrap();
    let mut rng = Rng(seed | 1);
    let objects: Vec<(ObjectId, Vec<u8>)> = (0..200u32)
        .map(|i| {
            let len = 1 + rng.below(6000) as usize;
            let content: Vec<u8> = (0..len)
                .map(|j| ((i as usize * 31 + j) % 256) as u8)
                .collect();
            (ObjectId::hash_blob(&content), content)
        })
        .collect();
    let upstream: HashMap<ObjectId, Vec<u8>> = objects.iter().cloned().collect();
    let mut inserted = std::collections::HashSet::new();
    let mut fetches = 0u64;
    for _ in 0..ops {
        let (id, content) = &objects[rng.below(objects.len() as u64) as usize];
        match rng.below(4) {
            0 => {
                let p = if rng.below(2) == 0 {
                    raw(content)
                } else {
                    zst(content)
                };
                c.insert(id, p).unwrap();
                inserted.insert(*id);
            }
            1 => {
                let got = read_through(&c, &upstream, id, &mut fetches);
                assert_eq!(&got[..], &content[..]);
                inserted.insert(*id);
            }
            2 => match c.get(id).unwrap() {
                Some(b) => assert_eq!(&b[..], &content[..]),
                None => assert!(!c.contains(id)),
            },
            _ => {
                if rng.below(20) == 0 {
                    c.remove(id).unwrap();
                }
            }
        }
        assert!(c.usage() <= max, "usage {} exceeds {}", c.usage(), max);
    }
    assert_eq!(c.usage(), disk_bytes(dir.path()));
    // Evicted ids read back as None and are re-fetched transparently.
    let mut evicted = 0;
    for id in &inserted {
        if !c.contains(id) {
            evicted += 1;
            assert_eq!(c.get(id).unwrap(), None);
            let mut n = 0;
            let got = read_through(&c, &upstream, id, &mut n);
            assert_eq!(&got[..], &upstream[id][..]);
            assert_eq!(n, 1);
            assert!(c.usage() <= max);
        }
    }
    assert!(evicted > 0, "workload must exercise eviction");
    assert!(fetches > 0);
}

#[test]
fn randomized_workload_respects_capacity() {
    let seeds = scion_core::test_support::proptest_cases(4);
    for seed in 0..u64::from(seeds) {
        randomized_workload(0x9e37_79b9_7f4a_7c15 ^ seed, 1500);
    }
}

#[test]
#[ignore = "spec-sized; run with SCION_FULL=1"]
fn full_randomized_workload_respects_capacity() {
    for seed in 0..64u64 {
        randomized_workload(0x1234_5678 ^ seed, 20_000);
    }
}

#[test]
fn concurrent_inserts_and_reads_keep_usage_exact() {
    let dir = tempfile::tempdir().unwrap();
    let max = 32 * 1024;
    let c = std::sync::Arc::new(DiskCache::open(dir.path(), max, 0.8, true).unwrap());
    let objects: std::sync::Arc<Vec<(ObjectId, Vec<u8>)>> = std::sync::Arc::new(
        (0..64u32)
            .map(|i| {
                let content = vec![i as u8; 500 + (i as usize * 37) % 1500];
                (ObjectId::hash_blob(&content), content)
            })
            .collect(),
    );
    let threads: Vec<_> = (0..8u64)
        .map(|t| {
            let c = c.clone();
            let objects = objects.clone();
            std::thread::spawn(move || {
                let mut rng = Rng(t * 7919 + 1);
                for _ in 0..400 {
                    let (id, content) = &objects[rng.below(objects.len() as u64) as usize];
                    if rng.below(2) == 0 {
                        c.insert(id, raw(content)).unwrap();
                    } else if let Some(b) = c.get(id).unwrap() {
                        assert_eq!(&b[..], &content[..]);
                    }
                    assert!(c.usage() <= max);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(c.usage(), disk_bytes(dir.path()));
    let tmp: Vec<_> = all_files(dir.path())
        .into_iter()
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(".tmp"))
        .collect();
    assert!(tmp.is_empty());
}
