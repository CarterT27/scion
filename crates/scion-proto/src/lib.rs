//! The `scion.v1` RPC surface (SPEC 7, PLAN 2.13).
//!
//! - [`v1`]: generated protobuf messages and tonic clients/servers for the six services.
//! - [`convert`]: domain <-> proto conversions.
//! - [`status_from_error`] / [`error_from_status`]: the SPEC 7.2 error mapping.
//! - Epoch trailers (SPEC 7.1): [`EpochTrailerLayer`], [`read_epochs`], [`write_epochs`].
//! - [`ScionClient`]: a client over a Unix socket or HTTPS (SPEC 7.1, 11).
//! - [`raw::RawCodec`]: byte-level codec for verbatim forwarding by the daemon (SPEC 7.8).

/// Generated `scion.v1` protobuf types and tonic services (SPEC 7).
#[allow(
    missing_docs,
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]
pub mod v1 {
    tonic::include_proto!("scion.v1");

    /// Encoded `FileDescriptorSet` of `scion.proto`, used to enumerate every RPC (SPEC 19.7).
    pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("scion_descriptor");
}

mod client;
pub mod convert;
mod endpoint;
pub mod raw;
mod status;
mod trailers;

pub use client::{BearerInterceptor, EpochCapture, ScionClient, Transport};
pub use endpoint::{Endpoint, TlsOptions};
pub use status::{error_from_status, status_from_error, tonic_code};
pub use trailers::{
    EpochSource, EpochTrailerLayer, Epochs, TRAILER_POLICY_EPOCH, TRAILER_REDACTION_EPOCH,
    TRAILER_TIER, read_epochs, write_epochs,
};

#[cfg(test)]
mod tests {
    use super::v1;
    use prost::Message;

    #[test]
    fn descriptor_set_lists_all_services() {
        let fds = prost_types::FileDescriptorSet::decode(v1::FILE_DESCRIPTOR_SET).unwrap();
        let file = fds
            .file
            .iter()
            .find(|f| f.package() == "scion.v1")
            .expect("scion.v1 file");
        let mut services: Vec<(&str, usize)> = file
            .service
            .iter()
            .map(|s| (s.name(), s.method.len()))
            .collect();
        services.sort();
        assert_eq!(
            services,
            vec![
                ("AdminService", 4),
                ("AuthService", 3),
                ("ObjectService", 4),
                ("QueryService", 10),
                ("RefService", 4),
                ("WriteService", 5),
            ]
        );
    }

    #[test]
    fn messages_round_trip() {
        let m = v1::GetObjectsRequest {
            ids: vec![vec![1; 34]],
            offset: Some(3),
            length: None,
        };
        let b = m.encode_to_vec();
        assert_eq!(v1::GetObjectsRequest::decode(b.as_slice()).unwrap(), m);
        assert_eq!(v1::ItemStatus::Redacted as i32, 4);
        assert_eq!(v1::Encoding::Zstd as i32, 2);
        assert_eq!(v1::AccessClass::Restricted as i32, 2);
    }
}
