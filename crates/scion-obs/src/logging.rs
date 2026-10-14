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
use tracing_subscriber::layer::{Layer, SubscriberExt};
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

fn stderr_layer(format: LogFormat) -> BoxedLayer {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match format {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_writer(std::io::stderr)
            .with_filter(filter)
            .boxed(),
        LogFormat::Text => tracing_subscriber::fmt::layer()
            .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
            .with_writer(std::io::stderr)
            .with_filter(filter)
            .boxed(),
    }
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
        tracing::subscriber::set_global_default(subscriber).is_ok()
    })
}

/// Install the global tracing subscriber (SPEC 15.1): JSON (one event per line) or text on
/// stderr, filtered by `RUST_LOG` (default `info`). Idempotent; if a subscriber is already
/// installed, this is a no-op.
pub fn init_logging(format: LogFormat) -> LogGuard {
    ensure_installed(Some(format));
    LogGuard
}
