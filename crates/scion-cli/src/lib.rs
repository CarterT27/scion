//! The `scion` command-line client library (SPEC 11.8; PLAN 2.18, 6.15). `main()` calls
//! [`run`], which is also the testable entry point.
#![warn(missing_docs)]

mod cli;
pub mod output;

pub use cli::{CliIo, run};
