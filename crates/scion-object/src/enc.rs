//! SPEC 4.1 canonical encoding primitives (P01).
//!
//! All decoding errors are INVALID_ARGUMENT with the decoding subcodes of
//! [`scion_core::subcode`].
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

use scion_core::{ObjectId, Result};

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
        todo!("P01")
    }

    /// `uvarint`: minimal unsigned LEB128 (at most 10 bytes).
    pub fn uvarint(&mut self, v: u64) {
        todo!("P01")
    }

    /// `svarint`: zigzag-mapped uvarint.
    pub fn svarint(&mut self, v: i64) {
        todo!("P01")
    }

    /// `bytes`: uvarint length then the octets.
    pub fn bytes(&mut self, b: &[u8]) {
        todo!("P01")
    }

    /// Raw octets without a length prefix (fixed-size fields such as `change_id`).
    pub fn raw(&mut self, b: &[u8]) {
        todo!("P01")
    }

    /// `ObjectId`: 34 raw bytes (SPEC 4.2).
    pub fn id(&mut self, id: &ObjectId) {
        todo!("P01")
    }

    /// The encoded bytes.
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
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

    /// `u8`.
    pub fn u8(&mut self) -> Result<u8> {
        todo!("P01")
    }

    /// `uvarint`; rejects non-minimal encodings and values over 10 bytes / 64 bits.
    pub fn uvarint(&mut self) -> Result<u64> {
        todo!("P01")
    }

    /// `svarint`.
    pub fn svarint(&mut self) -> Result<i64> {
        todo!("P01")
    }

    /// `bytes`; rejects lengths exceeding the remaining input.
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        todo!("P01")
    }

    /// Exactly `N` raw octets.
    pub fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        todo!("P01")
    }

    /// `ObjectId` (34 bytes, validated algo and kind).
    pub fn id(&mut self) -> Result<ObjectId> {
        todo!("P01")
    }

    /// A `list<T>` count, bounded by the remaining input.
    pub fn list_len(&mut self) -> Result<usize> {
        todo!("P01")
    }

    /// An `option<T>` tag: 0 or 1, anything else invalid.
    pub fn option_tag(&mut self) -> Result<bool> {
        todo!("P01")
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// Succeeds only when every byte was consumed (TRAILING_BYTES otherwise).
    pub fn finish(self) -> Result<()> {
        todo!("P01")
    }
}
