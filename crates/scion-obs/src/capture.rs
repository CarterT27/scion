//! In-memory JSON log capture for log-scan tests (SPEC 19.17; PLAN 6.9; P04).
//!
//! [`LogCapture::install`] turns on the capture layer of the process-global subscriber (see
//! [`crate::init_logging`]). Captured events are JSON, one per line, with event fields
//! flattened to top-level keys. The buffer is shared by every handle in the process, so tests
//! running in parallel should plant unique needles.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tracing_subscriber::fmt::MakeWriter;

struct Sink {
    on: AtomicBool,
    buf: Arc<Mutex<Vec<u8>>>,
}

fn sink() -> &'static Sink {
    static SINK: OnceLock<Sink> = OnceLock::new();
    SINK.get_or_init(|| Sink {
        on: AtomicBool::new(false),
        buf: Arc::new(Mutex::new(Vec::new())),
    })
}

fn lock(buf: &Mutex<Vec<u8>>) -> MutexGuard<'_, Vec<u8>> {
    buf.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether a `LogCapture` has been installed in this process.
pub(crate) fn capture_enabled() -> bool {
    sink().on.load(Ordering::Relaxed)
}

/// `MakeWriter` for the capture layer: each event is formatted into a private buffer and
/// appended to the shared buffer in one piece when the writer drops, so lines never
/// interleave.
pub(crate) struct CaptureMakeWriter;

pub(crate) struct CaptureWriter {
    local: Vec<u8>,
}

impl io::Write for CaptureWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.local.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for CaptureWriter {
    fn drop(&mut self) {
        if !self.local.is_empty() && capture_enabled() {
            lock(&sink().buf).extend_from_slice(&self.local);
        }
    }
}

impl<'a> MakeWriter<'a> for CaptureMakeWriter {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter { local: Vec::new() }
    }
}

/// The byte patterns under which `needle` may appear in a JSON log line: raw, JSON-escaped,
/// and Rust-`Debug`-escaped then JSON-escaped (for `?field` values).
fn needle_variants(needle: &[u8]) -> Vec<Vec<u8>> {
    let mut out = vec![needle.to_vec()];
    if let Ok(s) = std::str::from_utf8(needle) {
        let json = |s: &str| -> Option<Vec<u8>> {
            let quoted = serde_json::to_string(s).ok()?;
            let inner = quoted.get(1..quoted.len().saturating_sub(1))?;
            Some(inner.as_bytes().to_vec())
        };
        if let Some(j) = json(s) {
            out.push(j);
        }
        let dbg = format!("{s:?}");
        if let Some(inner) = dbg.get(1..dbg.len().saturating_sub(1))
            && let Some(j) = json(inner)
        {
            out.push(j);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// A process-global in-memory JSON log layer (SPEC 19.17 log scans; PLAN 6.9).
pub struct LogCapture {
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl LogCapture {
    /// Install (process-global, idempotent) and return a handle (SPEC 19.17). If another
    /// global subscriber was installed before Scion's, nothing is captured.
    pub fn install() -> Self {
        let s = sink();
        s.on.store(true, Ordering::Relaxed);
        crate::logging::ensure_installed(None);
        LogCapture { buf: s.buf.clone() }
    }

    /// Captured events as JSON values (lines that are not JSON are skipped).
    pub fn events(&self) -> Vec<serde_json::Value> {
        let buf = lock(&self.buf);
        buf.split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_slice(l).ok())
            .collect()
    }

    /// The raw captured text.
    pub fn raw(&self) -> String {
        String::from_utf8_lossy(&lock(&self.buf)).into_owned()
    }

    /// Captured lines containing any of `needles` (secrets, content, raw paths), matched raw
    /// and in their JSON/`Debug`-escaped forms. Empty needles never match.
    pub fn find_needles(&self, needles: &[&[u8]]) -> Vec<String> {
        let variants: Vec<Vec<u8>> = needles
            .iter()
            .filter(|n| !n.is_empty())
            .flat_map(|n| needle_variants(n))
            .collect();
        let buf = lock(&self.buf);
        buf.split(|b| *b == b'\n')
            .filter(|line| variants.iter().any(|v| contains(line, v)))
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .collect()
    }

    /// Drop everything captured so far (SPEC 19.17).
    pub fn clear(&self) {
        lock(&self.buf).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_cover_escapes() {
        let v = needle_variants(b"a\"b\n");
        assert!(v.contains(&b"a\"b\n".to_vec()));
        assert!(v.contains(&br#"a\"b\n"#.to_vec()));
        // Debug form `a\"b\n` then JSON-escaped: `a\\\"b\\n`.
        assert!(v.contains(&br#"a\\\"b\\n"#.to_vec()));
        assert_eq!(needle_variants(&[0xff, 0x00]), vec![vec![0xff, 0x00]]);
    }

    #[test]
    fn contains_basics() {
        assert!(contains(b"hello world", b"lo w"));
        assert!(!contains(b"hello", b""));
        assert!(!contains(b"he", b"hello"));
    }
}
