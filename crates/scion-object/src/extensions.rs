//! SPEC 4.1 `Extensions`: strictly ascending `(tag, value)` pairs; odd tags are critical.
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::enc::Decoder;
use scion_core::Result;

/// SPEC 4.3.3 Meta extension tag 2 (ignorable): the 20-byte git commit id from import.
pub const META_EXT_GIT_COMMIT: u64 = 2;
/// Implementation-defined Meta extension tag 4 (ignorable): skipped gitlinks,
/// `list<(path bytes, 20-byte sha)>` (PLAN 6.4).
pub const META_EXT_SKIPPED_GITLINKS: u64 = 4;

/// Extension list with strictly ascending tags. Decoding rejects odd tags not listed as
/// known; unknown even tags are preserved byte for byte (SPEC 4.1).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Extensions(Vec<(u64, Vec<u8>)>);

impl Extensions {
    /// An empty extension list.
    pub fn new() -> Self {
        Extensions(Vec::new())
    }

    /// The value for `tag`, if present.
    pub fn get(&self, tag: u64) -> Option<&[u8]> {
        todo!("P01")
    }

    /// Insert or replace `tag`, keeping tags strictly ascending.
    pub fn insert(&mut self, tag: u64, value: Vec<u8>) -> Result<()> {
        todo!("P01")
    }

    /// Iterate `(tag, value)` in ascending tag order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &[u8])> {
        self.0.iter().map(|(t, v)| (*t, v.as_slice()))
    }

    /// Decode an extension list, rejecting unsorted/duplicate tags and odd tags not in
    /// `known_odd` (unknown_critical_extension).
    pub fn decode_with(d: &mut Decoder<'_>, known_odd: &[u64]) -> Result<Self> {
        todo!("P01")
    }
}
