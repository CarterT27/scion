//! Deterministic generators for property-style tests (SplitMix64; scion-proto has no proptest
//! dev-dependency).
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scion_core::{
    ChangeId, CleanEntry, Conflict, ConflictHunk, ConflictKind, ConflictReport, EntryInfo,
    ErrorCode, ErrorPayload, Kind, Limits, Mode, ObjectId, ScionError,
};
use scion_object::Timestamp;

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    pub fn bool(&mut self) -> bool {
        self.next() & 1 == 1
    }
    pub fn u32(&mut self) -> u32 {
        self.next() as u32
    }
    /// Mostly small values, sometimes extremes.
    pub fn u64_edgy(&mut self) -> u64 {
        match self.below(4) {
            0 => 0,
            1 => u64::MAX - self.below(3),
            2 => self.below(1000),
            _ => self.next(),
        }
    }
    pub fn bytes(&mut self, max: u64) -> Vec<u8> {
        let n = self.below(max + 1);
        (0..n).map(|_| self.next() as u8).collect()
    }
    pub fn string(&mut self, max: u64) -> String {
        const ALPHABET: &[&str] = &[
            "a",
            "Z",
            "0",
            "_",
            " ",
            "/",
            ":",
            "%",
            "é",
            "日",
            "\u{1F600}",
            "\n",
        ];
        let n = self.below(max + 1);
        (0..n)
            .map(|_| ALPHABET[self.below(ALPHABET.len() as u64) as usize])
            .collect()
    }
}

pub fn kind(r: &mut Rng) -> Kind {
    match r.below(5) {
        0 => Kind::Blob,
        1 => Kind::Tree,
        2 => Kind::Changeset,
        3 => Kind::Meta,
        _ => Kind::Derived(0x10 + r.below(0x70) as u8),
    }
}

pub fn oid(r: &mut Rng) -> ObjectId {
    let mut d = [0u8; 32];
    for b in d.iter_mut() {
        *b = r.next() as u8;
    }
    ObjectId::new(kind(r), d)
}

pub fn mode(r: &mut Rng) -> Mode {
    [Mode::File, Mode::Executable, Mode::Symlink, Mode::Tree][r.below(4) as usize]
}

pub fn entry(r: &mut Rng) -> EntryInfo {
    EntryInfo {
        mode: mode(r),
        id: oid(r),
        size: r.u64_edgy(),
    }
}

pub fn opt_entry(r: &mut Rng) -> Option<EntryInfo> {
    r.bool().then(|| entry(r))
}

pub fn range(r: &mut Rng) -> std::ops::Range<u32> {
    let a = r.u32() % 10_000;
    let b = a + r.u32() % 100;
    a..b
}

pub fn conflict_report(r: &mut Rng) -> ConflictReport {
    let clean_entries = (0..r.below(5))
        .map(|_| CleanEntry {
            path: r.bytes(20),
            entry: opt_entry(r),
        })
        .collect();
    let conflicts = (0..r.below(5))
        .map(|_| Conflict {
            path: r.bytes(20),
            kind: ConflictKind::ALL[r.below(ConflictKind::ALL.len() as u64) as usize],
            base: opt_entry(r),
            ours: opt_entry(r),
            theirs: opt_entry(r),
            hunks: (0..r.below(4))
                .map(|_| ConflictHunk {
                    base: range(r),
                    ours: range(r),
                    theirs: range(r),
                })
                .collect(),
        })
        .collect();
    ConflictReport {
        clean_entries,
        conflicts,
    }
}

/// One of every payload variant, chosen by `which` (0..7).
pub fn payload_variant(r: &mut Rng, which: u64) -> ErrorPayload {
    match which {
        0 => ErrorPayload::None,
        1 => ErrorPayload::Subcode(r.string(12)),
        2 => ErrorPayload::Limit {
            name: r.string(12),
            limit: r.u64_edgy(),
            actual: r.u64_edgy(),
        },
        3 => ErrorPayload::MergeConflict(Box::new(conflict_report(r))),
        4 => ErrorPayload::RefMismatch {
            current_version: r.u64_edgy(),
            current_target: r.bool().then(|| oid(r)),
        },
        5 => ErrorPayload::AlreadyLanded {
            source: oid(r),
            result: oid(r),
            version: r.u64_edgy(),
        },
        _ => ErrorPayload::Candidates((0..r.below(4)).map(|_| oid(r)).collect()),
    }
}

pub const PAYLOAD_VARIANTS: u64 = 7;

pub fn payload(r: &mut Rng) -> ErrorPayload {
    let w = r.below(PAYLOAD_VARIANTS);
    payload_variant(r, w)
}

pub fn error(r: &mut Rng) -> ScionError {
    let code = ErrorCode::ALL[r.below(ErrorCode::ALL.len() as u64) as usize];
    ScionError::new(code, r.string(30)).with_payload(payload(r))
}

pub fn limits(r: &mut Rng) -> Limits {
    Limits {
        max_blob_bytes: r.u64_edgy(),
        max_delta_entries: r.u64_edgy(),
        max_parents: r.u64_edgy(),
        max_message_bytes: r.u64_edgy(),
        merge_max_text_bytes: r.u64_edgy(),
    }
}

pub fn timestamp(r: &mut Rng) -> Timestamp {
    Timestamp {
        seconds: r.next() as i64,
        tz_offset_minutes: (r.below(2000) as i64) - 1000,
    }
}

pub fn change_id(r: &mut Rng) -> ChangeId {
    let mut c = [0u8; 16];
    for b in c.iter_mut() {
        *b = r.next() as u8;
    }
    ChangeId(c)
}
