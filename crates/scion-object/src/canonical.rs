//! The [`Canonical`] trait for structured objects (SPEC 4.1, 4.2).

use crate::enc::{Decoder, Encoder};
use scion_core::{Kind, ObjectId, Result};

/// A structured object with exactly one canonical byte string per logical value (SPEC 4.1).
pub trait Canonical: Sized {
    /// The object kind used in its identifier (SPEC 4.2).
    const KIND: Kind;

    /// Append the canonical encoding.
    fn encode_into(&self, e: &mut Encoder);

    /// Decode and validate one value, consuming exactly its bytes.
    fn decode_from(d: &mut Decoder<'_>) -> Result<Self>;

    /// The canonical encoding.
    fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        self.encode_into(&mut e);
        e.finish()
    }

    /// Decode a complete value, rejecting trailing bytes (SPEC 4.1).
    fn decode(b: &[u8]) -> Result<Self> {
        let mut d = Decoder::new(b);
        let v = Self::decode_from(&mut d)?;
        d.finish()?;
        Ok(v)
    }

    /// The identifier: BLAKE3 of the canonical encoding with [`Canonical::KIND`] (SPEC 4.2).
    fn id(&self) -> ObjectId {
        ObjectId::hash_encoded(Self::KIND, &self.encode())
    }
}
