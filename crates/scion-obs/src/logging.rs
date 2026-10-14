//! Process logging setup (SPEC 15.1; PLAN 6.9; P04).
//!
//! One process-global tracing subscriber is installed by whichever of [`init_logging`] or
//! [`crate::capture::LogCapture::install`] runs first. It always carries the in-memory capture
//! layer (inert until a `LogCapture` is installed, and then limited to DEBUG for `scion*`
//! targets and INFO elsewhere); `init_logging` adds the stderr layer in the requested format.

use crate::capture::{CaptureMakeWriter, capture_enabled};
use std::sync::OnceLock;
use tracing::Metadata;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry, filter};

/// Log output format (SPEC 15.1: JSON by default).
#[derive(Copy, Clone, Debug)]
pub enum LogFormat {
    /// One JSON event per line.
    Json,
    /// Human-readable text.
    Text,
}

/// Keeps logging installed while alive (SPEC 15.1). The subscriber is process-global and
/// stays installed for the life of the process; the guard exists so callers hold it in
/// `main`.
pub struct LogGuard;

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

/// The level the capture layer records for `meta`: DEBUG and above for Scion targets, INFO
/// and above for everything else.
fn capture_wants(meta: &Metadata<'_>) -> bool {
    let max = if meta.target().starts_with("scion") {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };
    *meta.level() <= max
}

/// The operator log layer in `format`, writing to `writer` (stderr in production; a buffer in
/// unit tests). JSON events are one object per line with the event fields flattened to
/// top-level keys (SPEC 15.1).
fn fmt_layer<W>(format: LogFormat, writer: W, ansi: bool) -> BoxedLayer
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    match format {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_ansi(false)
            .with_writer(writer)
            .boxed(),
        LogFormat::Text => tracing_subscriber::fmt::layer()
            .with_ansi(ansi)
            .with_writer(writer)
            .boxed(),
    }
}

fn stderr_layer(format: LogFormat) -> BoxedLayer {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stderr());
    fmt_layer(format, std::io::stderr, ansi)
        .with_filter(filter)
        .boxed()
}

fn capture_layer() -> BoxedLayer {
    let f = filter::filter_fn(|meta| capture_enabled() && capture_wants(meta))
        .with_max_level_hint(LevelFilter::DEBUG);
    tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true)
        .with_ansi(false)
        .with_writer(CaptureMakeWriter)
        .with_filter(f)
        .boxed()
}

/// Install the process-global subscriber once. `stderr` selects the stderr layer format for
/// the first installer; later calls are no-ops. Returns whether this process's global
/// subscriber is ours (false if some other subscriber was installed first).
pub(crate) fn ensure_installed(stderr: Option<LogFormat>) -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| {
        let layers: Vec<BoxedLayer> = match stderr {
            Some(format) => vec![stderr_layer(format), capture_layer()],
            None => vec![capture_layer()],
        };
        let subscriber = Registry::default().with(layers);
        // `try_init` also bridges `log` records from dependencies into tracing. It fails if
        // a global subscriber is already set (then ours is not installed) or if a `log`
        // logger is already set (then ours is installed but `log` records are not bridged).
        let already_set = tracing::dispatcher::has_been_set();
        if let Err(e) = subscriber.try_init() {
            if already_set {
                return false;
            }
            tracing::debug!(target: "scion::obs", error = %e, "log records are not bridged");
        }
        true
    })
}

/// Install the global tracing subscriber (SPEC 15.1): JSON (one event per line) or text on
/// stderr, filtered by `RUST_LOG` (default `info`). Idempotent; if a subscriber is already
/// installed, this is a no-op.
pub fn init_logging(format: LogFormat) -> LogGuard {
    ensure_installed(Some(format));
    LogGuard
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RpcLogEvent;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn emit_with(format: LogFormat) -> String {
        let buf = Buf::default();
        let writer = buf.clone();
        let subscriber = Registry::default().with(fmt_layer(format, move || writer.clone(), false));
        tracing::subscriber::with_default(subscriber, || {
            RpcLogEvent {
                request_id: "req-fmt-1".into(),
                rpc: "Land".into(),
                tier: "server",
                principal: Some("github:octocat".into()),
                repo: "monorepo".into(),
                status: "OK".into(),
                duration_ms: 3,
                ref_name: Some("main".into()),
                ref_version: Some(4),
                land_attempt: Some(1),
                changeset_id: Some("c1".into()),
            }
            .emit();
            tracing::warn!(target: "scion::test", multi = "line\nbreak", "second");
        });
        String::from_utf8(buf.0.lock().unwrap().clone()).unwrap()
    }

    /// SPEC 15.1: the operator JSON format is one object per line with the RPC fields as
    /// top-level keys, even when a value contains a newline.
    #[test]
    fn json_format_is_one_event_per_line() {
        let out = emit_with(LogFormat::Json);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        let e: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        for key in [
            "request_id",
            "rpc",
            "tier",
            "principal",
            "repo",
            "status",
            "duration_ms",
            "changeset_id",
            "ref",
            "ref_version",
            "land_attempt",
        ] {
            assert!(e.get(key).is_some(), "missing {key} in {e}");
        }
        assert_eq!(e["target"], "scion::rpc");
        assert_eq!(e["level"], "INFO");
        let w: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(w["multi"], "line\nbreak");
        assert!(!out.contains('\u{1b}'), "no ANSI escapes in JSON");
    }

    #[test]
    fn text_format_names_fields() {
        let out = emit_with(LogFormat::Text);
        assert!(out.contains("request_id=\"req-fmt-1\"") || out.contains("request_id=req-fmt-1"));
        assert!(
            out.contains("ref=\"main\"") || out.contains("ref=main"),
            "{out}"
        );
        assert!(!out.contains('\u{1b}'));
    }
}
