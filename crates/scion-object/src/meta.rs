//! SPEC 4.3.3 Meta.
#![allow(unused_variables)] // P00 stub: remove when implemented.

use crate::canonical::Canonical;
use crate::enc::{Decoder, Encoder};
use crate::extensions::Extensions;
use scion_core::{Kind, Result};

/// SPEC 4.3.3 `Timestamp { seconds: svarint, tz_offset_minutes: svarint }`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Timestamp {
    /// Seconds since the Unix epoch.
    pub seconds: i64,
    /// Timezone offset in minutes.
    pub tz_offset_minutes: i64,
}

/// SPEC 4.3.3 Meta, a separate object so it can be redacted independently (SPEC 13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// Author, conventionally `Name <email>`.
    pub author: Vec<u8>,
    /// Author time.
    pub author_time: Timestamp,
    /// Committer (populated by import; absent for native changesets).
    pub committer: Option<Vec<u8>>,
    /// Committer time.
    pub committer_time: Option<Timestamp>,
    /// Principal id that created the changeset (server-set).
    pub creator: Vec<u8>,
    /// Message bytes.
    pub message: Vec<u8>,
    /// Extensions; tag 2 (ignorable) holds the git commit id from import.
    pub extensions: Extensions,
}

impl Canonical for Meta {
    const KIND: Kind = Kind::Meta;

    fn encode_into(&self, e: &mut Encoder) {
        todo!("P01")
    }

    fn decode_from(d: &mut Decoder<'_>) -> Result<Self> {
        todo!("P01")
    }
}
