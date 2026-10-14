//! Redaction mechanics (SPEC 13, 18.8; PLAN 2.8): quarantine, unredact, purge and the
//! background mover/purge scheduler. Authorization is checked by the server, not here.
#![warn(missing_docs)]

mod config;
mod redactor;

pub use config::{RedactOutcome, RedactionConfig};
pub use redactor::Redactor;
