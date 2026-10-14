//! SPEC 4.1 canonical encoding primitives (P01).
//!
//! All decoding errors are INVALID_ARGUMENT with the decoding subcodes of
//! [`scion_core::subcode`]:
//!
//! | Violation | Subcode |
//! |---|---|
//! | varint with a redundant trailing zero group | `non_minimal_varint` |
//! | varint longer than 10 bytes or above `u64::MAX` | `varint_overflow` |
//! | input ends early; `bytes`/`list` length above the remaining input | `length_overrun` |
//! | bytes left after the value | `trailing_bytes` |
//! | `option` tag other than 0 or 1 | `invalid_kind` (an out-of-range discriminator) |
//! | id with a bad algo / kind byte | `invalid_algo` / `invalid_kind` |

use scion_core::{ObjectId, Result, ScionError, subcode};

/// Maximum length of a SPEC 4.1 `uvarint` in bytes.
pub const MAX_VARINT_LEN: usize = 10;

/// Upper bound on list pre-allocation while decoding, so a forged count cannot force a
/// large allocation before the elements are actually read.
pub(crate) const PREALLOC_CAP: usize = 1024;

/// Appends SPEC 4.1 primitives to a byte buffer.
#[derive(Debug, Default)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    /// An empty encoder.
    pub fn new() -> Self {
        Encoder { buf: Vec::new() }
    }

    /// `u8`: one byte.
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    /// `uvarint`: minimal unsigned LEB128 (at most 10 bytes).
    pub fn uvarint(&mut self, mut v: u64) {
        loop {
            let low = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(low);
                return;
            }
            self.buf.push(low | 0x80);
        }
    }

    /// `svarint`: zigzag-mapped uvarint.
    pub fn svarint(&mut self, v: i64) {
        self.uvarint(zigzag(v));
    }

    /// `bytes`: uvarint length then the octets.
    pub fn bytes(&mut self, b: &[u8]) {
        self.uvarint(b.len() as u64);
        self.buf.extend_from_slice(b);
    }

    /// Raw octets without a length prefix (fixed-size fields such as `change_id`).
    pub fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// `ObjectId`: 34 raw bytes (SPEC 4.2).
    pub fn id(&mut self, id: &ObjectId) {
        self.buf.extend_from_slice(id.as_bytes());
    }

    /// The encoded bytes.
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// Zigzag mapping of SPEC 4.1 `svarint`: 0, -1, 1, -2, ... map to 0, 1, 2, 3, ...
pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// Inverse of [`zigzag`].
pub fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

fn overrun(what: &str, need: u64, have: usize) -> ScionError {
    ScionError::invalid(
        subcode::LENGTH_OVERRUN,
        format!("{what}: need {need} bytes, {have} remaining"),
    )
}

/// Reads SPEC 4.1 primitives from a byte slice, rejecting every non-canonical form.
#[derive(Debug)]
pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    /// A decoder over `b`.
    pub fn new(b: &'a [u8]) -> Self {
        Decoder { buf: b, pos: 0 }
    }

    /// Take exactly `n` bytes (LENGTH_OVERRUN when fewer remain).
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8]> {
        let rest = self.buf.get(self.pos..).unwrap_or(&[]);
        match rest.get(..n) {
            Some(s) => {
                self.pos += n;
                Ok(s)
            }
            None => Err(overrun(what, n as u64, rest.len())),
        }
    }

    /// `u8`.
    pub fn u8(&mut self) -> Result<u8> {
        let s = self.take(1, "u8")?;
        Ok(s.first().copied().unwrap_or(0))
    }

    /// `uvarint`; rejects non-minimal encodings and values over 10 bytes / 64 bits.
    pub fn uvarint(&mut self) -> Result<u64> {
        let mut v: u64 = 0;
        for i in 0..MAX_VARINT_LEN {
            let b = self.u8()?;
            if i == MAX_VARINT_LEN - 1 && b > 1 {
                // The 10th group carries only bit 63 and may not continue.
                return Err(ScionError::invalid(
                    subcode::VARINT_OVERFLOW,
                    "uvarint exceeds 64 bits or 10 bytes",
                ));
            }
            v |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                if i > 0 && b == 0 {
                    return Err(ScionError::invalid(
                        subcode::NON_MINIMAL_VARINT,
                        "uvarint is not minimally encoded",
                    ));
                }
                return Ok(v);
            }
        }
        // Unreachable: the 10th byte either ends the varint or fails above.
        Err(ScionError::invalid(
            subcode::VARINT_OVERFLOW,
            "uvarint exceeds 10 bytes",
        ))
    }

    /// `svarint`.
    pub fn svarint(&mut self) -> Result<i64> {
        Ok(unzigzag(self.uvarint()?))
    }

    /// `bytes`; rejects lengths exceeding the remaining input.
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.uvarint()?;
        let remaining = self.remaining();
        if len > remaining as u64 {
            return Err(overrun("bytes length", len, remaining));
        }
        self.take(len as usize, "bytes")
    }

    /// Exactly `N` raw octets.
    pub fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        let s = self.take(N, "fixed")?;
        let mut out = [0u8; N];
        out.copy_from_slice(s);
        Ok(out)
    }

    /// `ObjectId` (34 bytes, validated algo and kind).
    pub fn id(&mut self) -> Result<ObjectId> {
        let raw = self.fixed::<34>()?;
        ObjectId::from_bytes(&raw)
    }

    /// A `list<T>` count, bounded by the remaining input.
    pub fn list_len(&mut self) -> Result<usize> {
        let n = self.uvarint()?;
        let remaining = self.remaining();
        // Every element occupies at least one byte.
        if n > remaining as u64 {
            return Err(overrun("list count", n, remaining));
        }
        Ok(n as usize)
    }

    /// An `option<T>` tag: 0 or 1, anything else invalid.
    pub fn option_tag(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            t => Err(ScionError::invalid(
                subcode::INVALID_KIND,
                format!("option tag must be 0 or 1, got {t}"),
            )),
        }
    }

    /// `format_version: uvarint` that must equal `expected` (UNKNOWN_FORMAT_VERSION).
    pub fn format_version(&mut self, expected: u64) -> Result<()> {
        let v = self.uvarint()?;
        if v != expected {
            return Err(ScionError::invalid(
                subcode::UNKNOWN_FORMAT_VERSION,
                format!("unknown format_version {v}"),
            ));
        }
        Ok(())
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// Succeeds only when every byte was consumed (TRAILING_BYTES otherwise).
    pub fn finish(self) -> Result<()> {
        match self.remaining() {
            0 => Ok(()),
            n => Err(ScionError::invalid(
                subcode::TRAILING_BYTES,
                format!("{n} trailing bytes"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::{ErrorCode, Kind};

    fn enc_u(v: u64) -> Vec<u8> {
        let mut e = Encoder::new();
        e.uvarint(v);
        e.finish()
    }

    fn sub(r: Result<impl std::fmt::Debug>) -> String {
        let e = r.unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        e.subcode().unwrap().to_owned()
    }

    #[test]
    fn uvarint_known_values() {
        assert_eq!(enc_u(0), [0x00]);
        assert_eq!(enc_u(1), [0x01]);
        assert_eq!(enc_u(127), [0x7f]);
        assert_eq!(enc_u(128), [0x80, 0x01]);
        assert_eq!(enc_u(300), [0xac, 0x02]);
        let max = enc_u(u64::MAX);
        assert_eq!(max.len(), 10);
        assert_eq!(*max.last().unwrap(), 0x01);
        for v in [0, 1, 127, 128, 300, 1 << 35, u64::MAX - 1, u64::MAX] {
            let b = enc_u(v);
            let mut d = Decoder::new(&b);
            assert_eq!(d.uvarint().unwrap(), v);
            d.finish().unwrap();
        }
    }

    #[test]
    fn uvarint_rejections() {
        assert_eq!(
            sub(Decoder::new(&[0x80, 0x00]).uvarint()),
            subcode::NON_MINIMAL_VARINT
        );
        assert_eq!(
            sub(Decoder::new(&[0x81, 0x80, 0x00]).uvarint()),
            subcode::NON_MINIMAL_VARINT
        );
        // 11 bytes.
        let mut eleven = vec![0xffu8; 10];
        eleven.push(0x01);
        assert_eq!(
            sub(Decoder::new(&eleven).uvarint()),
            subcode::VARINT_OVERFLOW
        );
        // 10 bytes but above u64::MAX.
        let mut big = vec![0xffu8; 9];
        big.push(0x02);
        assert_eq!(sub(Decoder::new(&big).uvarint()), subcode::VARINT_OVERFLOW);
        // 10th byte zero is non-minimal.
        let mut nm = vec![0xffu8; 9];
        nm.push(0x00);
        assert_eq!(
            sub(Decoder::new(&nm).uvarint()),
            subcode::NON_MINIMAL_VARINT
        );
        // truncated
        assert_eq!(
            sub(Decoder::new(&[0x80]).uvarint()),
            subcode::LENGTH_OVERRUN
        );
        assert_eq!(sub(Decoder::new(&[]).uvarint()), subcode::LENGTH_OVERRUN);
    }

    #[test]
    fn svarint_zigzag() {
        for (v, u) in [(0i64, 0u64), (-1, 1), (1, 2), (-2, 3), (2, 4)] {
            assert_eq!(zigzag(v), u);
            assert_eq!(unzigzag(u), v);
        }
        for v in [i64::MIN, i64::MAX, -12345678, 98765] {
            let mut e = Encoder::new();
            e.svarint(v);
            let b = e.finish();
            let mut d = Decoder::new(&b);
            assert_eq!(d.svarint().unwrap(), v);
            d.finish().unwrap();
        }
    }

    #[test]
    fn bytes_list_option_fixed() {
        let mut e = Encoder::new();
        e.bytes(b"abc");
        e.u8(1);
        e.raw(&[9, 9]);
        let b = e.finish();
        assert_eq!(b, [3, b'a', b'b', b'c', 1, 9, 9]);
        let mut d = Decoder::new(&b);
        assert_eq!(d.bytes().unwrap(), b"abc");
        assert!(d.option_tag().unwrap());
        assert_eq!(d.fixed::<2>().unwrap(), [9, 9]);
        d.finish().unwrap();

        assert_eq!(
            sub(Decoder::new(&[4, 1, 2, 3]).bytes()),
            subcode::LENGTH_OVERRUN
        );
        assert_eq!(
            sub(Decoder::new(&[3, 1, 2]).list_len()),
            subcode::LENGTH_OVERRUN
        );
        assert_eq!(Decoder::new(&[2, 1, 2]).list_len().unwrap(), 2);
        assert_eq!(sub(Decoder::new(&[2]).option_tag()), subcode::INVALID_KIND);
        assert!(!Decoder::new(&[0]).option_tag().unwrap());
        assert_eq!(
            sub(Decoder::new(&[1]).fixed::<2>()),
            subcode::LENGTH_OVERRUN
        );
        assert_eq!(sub(Decoder::new(&[1, 2]).finish()), subcode::TRAILING_BYTES);
        // Huge length does not overflow.
        let mut e = Encoder::new();
        e.uvarint(u64::MAX);
        assert_eq!(
            sub(Decoder::new(&e.finish()).bytes()),
            subcode::LENGTH_OVERRUN
        );
    }

    #[test]
    fn ids() {
        let id = ObjectId::new(Kind::Tree, [7; 32]);
        let mut e = Encoder::new();
        e.id(&id);
        let b = e.finish();
        assert_eq!(b.len(), 34);
        assert_eq!(Decoder::new(&b).id().unwrap(), id);
        let mut bad = b.clone();
        bad[0] = 2;
        assert_eq!(sub(Decoder::new(&bad).id()), subcode::INVALID_ALGO);
        let mut bad = b.clone();
        bad[1] = 0x05;
        assert_eq!(sub(Decoder::new(&bad).id()), subcode::INVALID_KIND);
        assert_eq!(sub(Decoder::new(&b[..33]).id()), subcode::LENGTH_OVERRUN);
    }

    #[test]
    fn format_version() {
        Decoder::new(&[1]).format_version(1).unwrap();
        assert_eq!(
            sub(Decoder::new(&[2]).format_version(1)),
            subcode::UNKNOWN_FORMAT_VERSION
        );
    }
}
