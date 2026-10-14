//! SPEC 4.3.3 Meta.

use crate::canonical::Canonical;
use crate::enc::{Decoder, Encoder};
use crate::extensions::Extensions;
use scion_core::{Kind, Result};

/// SPEC 4.3.3 Meta `format_version`.
const META_FORMAT_VERSION: u64 = 1;

/// SPEC 4.3.3 `Timestamp { seconds: svarint, tz_offset_minutes: svarint }`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Timestamp {
    /// Seconds since the Unix epoch.
    pub seconds: i64,
    /// Timezone offset in minutes.
    pub tz_offset_minutes: i64,
}

impl Timestamp {
    /// Append the SPEC 4.3.3 encoding (two svarints).
    pub fn encode_into(&self, e: &mut Encoder) {
        e.svarint(self.seconds);
        e.svarint(self.tz_offset_minutes);
    }

    /// Decode a SPEC 4.3.3 Timestamp.
    pub fn decode_from(d: &mut Decoder<'_>) -> Result<Self> {
        Ok(Timestamp {
            seconds: d.svarint()?,
            tz_offset_minutes: d.svarint()?,
        })
    }
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
        e.uvarint(META_FORMAT_VERSION);
        e.bytes(&self.author);
        self.author_time.encode_into(e);
        match &self.committer {
            Some(c) => {
                e.u8(1);
                e.bytes(c);
            }
            None => e.u8(0),
        }
        match &self.committer_time {
            Some(t) => {
                e.u8(1);
                t.encode_into(e);
            }
            None => e.u8(0),
        }
        e.bytes(&self.creator);
        e.bytes(&self.message);
        self.extensions.encode_into(e);
    }

    fn decode_from(d: &mut Decoder<'_>) -> Result<Self> {
        d.format_version(META_FORMAT_VERSION)?;
        let author = d.bytes()?.to_vec();
        let author_time = Timestamp::decode_from(d)?;
        let committer = if d.option_tag()? {
            Some(d.bytes()?.to_vec())
        } else {
            None
        };
        let committer_time = if d.option_tag()? {
            Some(Timestamp::decode_from(d)?)
        } else {
            None
        };
        let creator = d.bytes()?.to_vec();
        let message = d.bytes()?.to_vec();
        let extensions = Extensions::decode_with(d, &[])?;
        Ok(Meta {
            author,
            author_time,
            committer,
            committer_time,
            creator,
            message,
            extensions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_layout() {
        let m = Meta {
            author: b"a".to_vec(),
            author_time: Timestamp {
                seconds: -1,
                tz_offset_minutes: 60,
            },
            committer: None,
            committer_time: None,
            creator: b"c".to_vec(),
            message: b"m".to_vec(),
            extensions: Extensions::new(),
        };
        let b = m.encode();
        assert_eq!(
            b,
            [1, 1, b'a', 0x01, 0x78, 0, 0, 1, b'c', 1, b'm', 0],
            "{b:x?}"
        );
        assert_eq!(Meta::decode(&b).unwrap(), m);
        let mut with = m.clone();
        with.committer = Some(b"x".to_vec());
        with.committer_time = Some(Timestamp {
            seconds: 5,
            tz_offset_minutes: 0,
        });
        assert_eq!(Meta::decode(&with.encode()).unwrap(), with);
        assert_ne!(with.id(), m.id());
    }
}
