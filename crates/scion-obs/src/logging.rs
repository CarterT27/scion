//! Process logging setup (SPEC 15.1; P04).
#![allow(unused_variables)] // P00 stub: remove when implemented.

/// Log output format.
#[derive(Copy, Clone, Debug)]
pub enum LogFormat {
    /// One JSON event per line.
    Json,
    /// Human-readable text.
    Text,
}

/// Keeps logging installed while alive.
pub struct LogGuard;

/// Install the global tracing subscriber.
pub fn init_logging(format: LogFormat) -> LogGuard {
    todo!("P04")
}
