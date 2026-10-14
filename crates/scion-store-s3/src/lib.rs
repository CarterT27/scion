//! S3-compatible [`scion_store::BlobStore`] backend via `object_store` (SPEC 5.1; PLAN 2.4).
#![warn(missing_docs)]

mod store;

pub use store::{S3BlobStore, S3Config, test_config};
