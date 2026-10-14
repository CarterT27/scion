//! In-memory JSON log capture for log-scan tests (SPEC 19.17; PLAN 6.9; P04).
#![allow(unused_variables, dead_code)] // P00 stub: remove when implemented.

/// A process-global in-memory JSON log layer.
pub struct LogCapture {
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl LogCapture {
    /// Install (process-global, idempotent) and return a handle.
    pub fn install() -> Self {
        todo!("P04")
    }

    /// Captured events as JSON values.
    pub fn events(&self) -> Vec<serde_json::Value> {
        todo!("P04")
    }

    /// The raw captured text.
    pub fn raw(&self) -> String {
        todo!("P04")
    }

    /// Captured lines containing any of `needles` (secrets, content, raw paths).
    pub fn find_needles(&self, needles: &[&[u8]]) -> Vec<String> {
        todo!("P04")
    }

    /// Drop everything captured so far.
    pub fn clear(&self) {
        todo!("P04")
    }
}
