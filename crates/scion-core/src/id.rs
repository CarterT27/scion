//! Object identifiers, kinds and change ids (SPEC 4.2, 4.3.5; PLAN 2.1, 6.6).

use crate::error::{Result, ScionError};
use crate::subcode;
use std::fmt;
use std::str::FromStr;

/// SPEC 4.2: algorithm byte for BLAKE3-256.
pub const ALGO_BLAKE3: u8 = 0x01;

/// The derived kind used by history nodes (SPEC 9.2).
const HISTORY_NODE_KIND: u8 = 0x10;

/// SPEC 4.2 object kinds.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// `0x01` raw file content.
    Blob,
    /// `0x02` directory listing.
    Tree,
    /// `0x03` changeset.
    Changeset,
    /// `0x04` changeset metadata.
    Meta,
    /// `0x10..=0x7F` derived objects (SPEC 9).
    Derived(u8),
}

impl Kind {
    /// The kind byte (SPEC 4.2).
    pub fn byte(self) -> u8 {
        match self {
            Kind::Blob => 0x01,
            Kind::Tree => 0x02,
            Kind::Changeset => 0x03,
            Kind::Meta => 0x04,
            Kind::Derived(b) => b,
        }
    }

    /// Parse a kind byte; anything outside SPEC 4.2 is INVALID_ARGUMENT/invalid_kind.
    pub fn from_byte(b: u8) -> Result<Self> {
        match b {
            0x01 => Ok(Kind::Blob),
            0x02 => Ok(Kind::Tree),
            0x03 => Ok(Kind::Changeset),
            0x04 => Ok(Kind::Meta),
            0x10..=0x7F => Ok(Kind::Derived(b)),
            _ => Err(ScionError::invalid(
                subcode::INVALID_KIND,
                format!("invalid object kind byte 0x{b:02x}"),
            )),
        }
    }

    /// Text-form letter: `b t c m x` (SPEC 4.2).
    pub fn letter(self) -> char {
        match self {
            Kind::Blob => 'b',
            Kind::Tree => 't',
            Kind::Changeset => 'c',
            Kind::Meta => 'm',
            Kind::Derived(_) => 'x',
        }
    }

    /// Dense index for per-kind counters: Blob 0, Tree 1, Changeset 2, Meta 3, Derived 4.
    pub fn index(self) -> usize {
        match self {
            Kind::Blob => 0,
            Kind::Tree => 1,
            Kind::Changeset => 2,
            Kind::Meta => 3,
            Kind::Derived(_) => 4,
        }
    }

    /// Infallible mapping for bytes already validated by [`Kind::from_byte`].
    fn from_valid_byte(b: u8) -> Self {
        match b {
            0x01 => Kind::Blob,
            0x02 => Kind::Tree,
            0x03 => Kind::Changeset,
            0x04 => Kind::Meta,
            other => Kind::Derived(other),
        }
    }
}

/// SPEC 4.2 `ObjectId = algo (u8) || kind (u8) || digest (32 bytes)`.
///
/// `Display`/`Debug` print the text form; serde uses the text form.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId([u8; 34]);

fn bad_text(msg: impl Into<String>) -> ScionError {
    ScionError::invalid(subcode::BAD_ID_TEXT, msg)
}

fn parse_lower_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut out = [0u8; N];
    hex::decode_to_slice(s, &mut out).ok()?;
    Some(out)
}

impl ObjectId {
    /// Build an id from a kind and a BLAKE3 digest (algo `0x01`).
    pub fn new(kind: Kind, digest: [u8; 32]) -> Self {
        let mut b = [0u8; 34];
        b[0] = ALGO_BLAKE3;
        b[1] = kind.byte();
        b[2..].copy_from_slice(&digest);
        ObjectId(b)
    }

    /// Parse 34 raw bytes, validating length (bad_id_text), algo (invalid_algo) and kind
    /// (invalid_kind).
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        let arr: [u8; 34] = b
            .try_into()
            .map_err(|_| bad_text(format!("object id must be 34 bytes, got {}", b.len())))?;
        if arr[0] != ALGO_BLAKE3 {
            return Err(ScionError::invalid(
                subcode::INVALID_ALGO,
                format!("unknown hash algorithm 0x{:02x}", arr[0]),
            ));
        }
        Kind::from_byte(arr[1])?;
        Ok(ObjectId(arr))
    }

    /// The 34 raw bytes.
    pub fn as_bytes(&self) -> &[u8; 34] {
        &self.0
    }

    /// The object kind.
    pub fn kind(&self) -> Kind {
        Kind::from_valid_byte(self.0[1])
    }

    /// The 32-byte BLAKE3 digest.
    pub fn digest(&self) -> &[u8; 32] {
        // A 34-byte array always has a trailing 32-byte chunk; the fallback is unreachable.
        self.0.last_chunk::<32>().unwrap_or(&[0u8; 32])
    }

    /// Blob id: BLAKE3 of the raw (uncompressed) content (SPEC 4.2).
    pub fn hash_blob(raw: &[u8]) -> Self {
        ObjectId::new(Kind::Blob, *blake3::hash(raw).as_bytes())
    }

    /// Id of a structured object: BLAKE3 of its canonical encoding (SPEC 4.2).
    pub fn hash_encoded(kind: Kind, canonical: &[u8]) -> Self {
        ObjectId::new(kind, *blake3::hash(canonical).as_bytes())
    }

    /// 64 lowercase hex digits of the digest.
    pub fn hex(&self) -> String {
        hex::encode(self.digest())
    }

    /// SPEC 4.2 text form: letter, algo digit, 64 hex. Derived kind 0x10 prints as
    /// `x1<64hex>`; other derived kinds as `x1<2-hex kind><64hex>` (PLAN 6.6).
    pub fn to_text(&self) -> String {
        let kind = self.kind();
        let mut s = String::with_capacity(68);
        s.push(kind.letter());
        s.push(char::from(b'0' + self.0[0]));
        if let Kind::Derived(k) = kind {
            if k != HISTORY_NODE_KIND {
                s.push_str(&format!("{k:02x}"));
            }
        }
        s.push_str(&self.hex());
        s
    }

    /// Parse the text form produced by [`ObjectId::to_text`]; INVALID_ARGUMENT/bad_id_text.
    pub fn parse_text(s: &str) -> Result<Self> {
        let b = s.as_bytes();
        if b.len() < 2 {
            return Err(bad_text("object id text too short"));
        }
        if b[1] != b'1' {
            return Err(bad_text("object id text must use algorithm 1"));
        }
        let rest = &s[2..];
        let kind = match b[0] {
            b'b' => Kind::Blob,
            b't' => Kind::Tree,
            b'c' => Kind::Changeset,
            b'm' => Kind::Meta,
            b'x' => {
                if rest.len() == 64 {
                    Kind::Derived(HISTORY_NODE_KIND)
                } else if rest.len() == 66 {
                    let [k] = parse_lower_hex::<1>(&rest[..2])
                        .ok_or_else(|| bad_text("bad derived kind in object id text"))?;
                    if k == HISTORY_NODE_KIND || !(0x10..=0x7F).contains(&k) {
                        return Err(bad_text("non-canonical derived kind in object id text"));
                    }
                    let digest = parse_lower_hex::<32>(&rest[2..])
                        .ok_or_else(|| bad_text("object id digest must be 64 lowercase hex"))?;
                    return Ok(ObjectId::new(Kind::Derived(k), digest));
                } else {
                    return Err(bad_text("bad derived object id text length"));
                }
            }
            _ => return Err(bad_text("unknown object kind letter")),
        };
        let digest = parse_lower_hex::<32>(rest)
            .ok_or_else(|| bad_text("object id digest must be 64 lowercase hex"))?;
        Ok(ObjectId::new(kind, digest))
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

impl FromStr for ObjectId {
    type Err = ScionError;
    fn from_str(s: &str) -> Result<Self> {
        ObjectId::parse_text(s)
    }
}

impl serde::Serialize for ObjectId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_text())
    }
}

impl<'de> serde::Deserialize<'de> for ObjectId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        ObjectId::parse_text(&s).map_err(serde::de::Error::custom)
    }
}

/// The empty tree id: `hash_encoded(Tree, [0x01, 0x00, 0x00])` (format_version 1, no
/// entries, no extensions; SPEC 4.3.2).
pub fn empty_tree_id() -> ObjectId {
    ObjectId::hash_encoded(Kind::Tree, &[0x01, 0x00, 0x00])
}

/// SPEC 4.3.5 ChangeId: 16 bytes, text form 32 lowercase hex digits.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChangeId(pub [u8; 16]);

impl ChangeId {
    /// 16 random bytes from the OS-seeded CSPRNG; the only OS-random use in production
    /// (PLAN 5.1).
    pub fn random() -> Self {
        ChangeId(rand::random::<[u8; 16]>())
    }

    /// 32 lowercase hex digits.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parse 32 lowercase hex digits; INVALID_ARGUMENT/bad_id_text.
    pub fn parse_hex(s: &str) -> Result<Self> {
        parse_lower_hex::<16>(s)
            .map(ChangeId)
            .ok_or_else(|| bad_text("change id must be 32 lowercase hex digits"))
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl serde::Serialize for ChangeId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> serde::Deserialize<'de> for ChangeId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        ChangeId::parse_hex(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    fn all_kinds() -> Vec<Kind> {
        let mut v = vec![Kind::Blob, Kind::Tree, Kind::Changeset, Kind::Meta];
        v.extend((0x10..=0x7Fu8).map(Kind::Derived));
        v
    }

    #[test]
    fn kind_bytes_round_trip() {
        for k in all_kinds() {
            assert_eq!(Kind::from_byte(k.byte()).unwrap(), k);
        }
        for b in [0x00u8, 0x05, 0x0f, 0x80, 0xff] {
            let e = Kind::from_byte(b).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument);
            assert_eq!(e.subcode(), Some(subcode::INVALID_KIND));
        }
        assert_eq!(Kind::Blob.letter(), 'b');
        assert_eq!(Kind::Tree.letter(), 't');
        assert_eq!(Kind::Changeset.letter(), 'c');
        assert_eq!(Kind::Meta.letter(), 'm');
        assert_eq!(Kind::Derived(0x10).letter(), 'x');
        assert_eq!(Kind::Derived(0x10).index(), 4);
        assert_eq!(Kind::Meta.index(), 3);
    }

    #[test]
    fn text_round_trip_every_kind() {
        let digest = *blake3::hash(b"scion").as_bytes();
        for k in all_kinds() {
            let id = ObjectId::new(k, digest);
            let text = id.to_text();
            assert_eq!(ObjectId::parse_text(&text).unwrap(), id, "{text}");
            assert_eq!(text.parse::<ObjectId>().unwrap(), id);
            assert_eq!(format!("{id}"), text);
            assert_eq!(format!("{id:?}"), text);
            assert_eq!(ObjectId::from_bytes(id.as_bytes()).unwrap(), id);
            assert_eq!(id.kind(), k);
            assert_eq!(id.digest(), &digest);
            assert_eq!(id.hex(), hex::encode(digest));
        }
        let c = ObjectId::new(Kind::Changeset, digest);
        assert_eq!(c.to_text(), format!("c1{}", hex::encode(digest)));
        let x = ObjectId::new(Kind::Derived(0x10), digest);
        assert_eq!(x.to_text(), format!("x1{}", hex::encode(digest)));
        let x2 = ObjectId::new(Kind::Derived(0x11), digest);
        assert_eq!(x2.to_text(), format!("x111{}", hex::encode(digest)));
    }

    #[test]
    fn equal_digest_different_kind_is_distinct() {
        let digest = [7u8; 32];
        let ids: Vec<ObjectId> = all_kinds()
            .into_iter()
            .map(|k| ObjectId::new(k, digest))
            .collect();
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                assert_ne!(a, b);
                assert_ne!(a.to_text(), b.to_text());
                assert_eq!(a.digest(), b.digest());
            }
        }
    }

    #[test]
    fn parse_text_rejects_bad_input() {
        let good = ObjectId::new(Kind::Tree, [0xab; 32]).to_text();
        let upper = good.to_uppercase();
        let bad = [
            "".to_string(),
            "c".to_string(),
            "c1".to_string(),
            format!("c2{}", &good[2..]),
            format!("z1{}", &good[2..]),
            format!("{}0", good),
            good[..good.len() - 1].to_string(),
            upper,
            format!("t1{}g", &good[2..good.len() - 1]),
            format!("x110{}", &good[2..]),
            format!("x180{}", &good[2..]),
            format!("x1{}", &good[3..]),
        ];
        for s in bad {
            let e = ObjectId::parse_text(&s).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument, "{s}");
            assert_eq!(e.subcode(), Some(subcode::BAD_ID_TEXT), "{s}");
        }
    }

    #[test]
    fn from_bytes_validates() {
        let id = ObjectId::new(Kind::Meta, [1; 32]);
        assert_eq!(
            ObjectId::from_bytes(&id.as_bytes()[..33])
                .unwrap_err()
                .subcode(),
            Some(subcode::BAD_ID_TEXT)
        );
        let mut b = *id.as_bytes();
        b[0] = 2;
        assert_eq!(
            ObjectId::from_bytes(&b).unwrap_err().subcode(),
            Some(subcode::INVALID_ALGO)
        );
        let mut b = *id.as_bytes();
        b[1] = 0x09;
        assert_eq!(
            ObjectId::from_bytes(&b).unwrap_err().subcode(),
            Some(subcode::INVALID_KIND)
        );
    }

    #[test]
    fn hashing() {
        assert_eq!(
            ObjectId::hash_blob(b"hi").digest(),
            blake3::hash(b"hi").as_bytes()
        );
        assert_eq!(ObjectId::hash_blob(b"hi").kind(), Kind::Blob);
        let e = empty_tree_id();
        assert_eq!(e.kind(), Kind::Tree);
        assert_eq!(e.digest(), blake3::hash(&[0x01, 0x00, 0x00]).as_bytes());
        assert_eq!(e, ObjectId::hash_encoded(Kind::Tree, &[1, 0, 0]));
    }

    #[test]
    fn serde_text_form() {
        let id = ObjectId::new(Kind::Changeset, [3; 32]);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{}\"", id.to_text()));
        assert_eq!(serde_json::from_str::<ObjectId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<ObjectId>("\"nope\"").is_err());
        let c = ChangeId([9; 16]);
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<ChangeId>(&json).unwrap(), c);
    }

    #[test]
    fn change_id_hex() {
        let c = ChangeId::random();
        assert_eq!(ChangeId::parse_hex(&c.to_hex()).unwrap(), c);
        assert_eq!(c.to_hex().len(), 32);
        assert_ne!(ChangeId::random(), ChangeId::random());
        for bad in [
            "",
            "00",
            &"A".repeat(32),
            &"0".repeat(31),
            &"0".repeat(33),
            &"g".repeat(32),
        ] {
            assert_eq!(
                ChangeId::parse_hex(bad).unwrap_err().subcode(),
                Some(subcode::BAD_ID_TEXT)
            );
        }
    }
}
