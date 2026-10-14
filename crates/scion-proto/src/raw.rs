//! Byte-level codec for verbatim forwarding (SPEC 7.8, PLAN 6.13; P08).

/// A `tonic::codec::Codec` whose `Encode` and `Decode` types are both `bytes::Bytes`, so the
/// daemon can forward non-object RPCs byte for byte.
#[derive(Clone, Copy, Debug, Default)]
pub struct RawCodec;
