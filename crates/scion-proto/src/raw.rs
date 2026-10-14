//! Byte-level codec for verbatim forwarding (SPEC 7.8, PLAN 6.13; P08).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use bytes::Bytes;
use tonic::Status;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};

/// A `tonic::codec::Codec` whose `Encode` and `Decode` types are both `bytes::Bytes`, so the
/// daemon can forward non-object RPCs byte for byte (SPEC 7.8, PLAN 6.13).
#[derive(Clone, Copy, Debug, Default)]
pub struct RawCodec;

/// Encoder half of [`RawCodec`]: writes the message bytes unchanged (SPEC 7.8).
#[derive(Clone, Copy, Debug, Default)]
pub struct RawEncoder;

/// Decoder half of [`RawCodec`]: yields the message bytes unchanged (SPEC 7.8).
#[derive(Clone, Copy, Debug, Default)]
pub struct RawDecoder;

impl Codec for RawCodec {
    type Encode = Bytes;
    type Decode = Bytes;
    type Encoder = RawEncoder;
    type Decoder = RawDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        RawEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        RawDecoder
    }
}

impl Encoder for RawEncoder {
    type Item = Bytes;
    type Error = Status;

    fn encode(&mut self, item: Bytes, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        todo!("P08")
    }
}

impl Decoder for RawDecoder {
    type Item = Bytes;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Bytes>, Status> {
        todo!("P08")
    }
}
