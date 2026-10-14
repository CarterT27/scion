//! Daemon startup and handle (SPEC 3.3, 10, 13.3, 17; PLAN 2.16, 6.13; P16).

mod fetch;
mod forward;
mod objects;
mod redaction;
mod state;

use crate::upstream::UpstreamConnector;
use scion_cache::DiskCache;
use scion_config::DaemonConfig;
use scion_core::{Clock, ErrorCode, Result, ScionError};
use scion_obs::Metrics;
use scion_proto::{Endpoint, EpochTrailerLayer};
use state::{DaemonEpochs, Inner};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// How long `shutdown` waits for in-flight calls before aborting the server task.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Observable daemon counters and state (SPEC 10, 13.3, 15.2).
#[derive(Clone, Debug, Default)]
pub struct DaemonStats {
    /// Requests sent upstream.
    pub upstream_requests: u64,
    /// Objects served from cache.
    pub cache_hits: u64,
    /// Objects fetched upstream.
    pub cache_misses: u64,
    /// True while in fail-closed mode (SPEC 13.3).
    pub fail_closed: bool,
    /// Local redaction epoch.
    pub redaction_epoch: u64,
    /// Last observed policy epoch.
    pub policy_epoch: u64,
}

/// Daemon entry point (SPEC 3.1 item 4, 10).
pub struct Daemon;

fn io_error(what: &str, path: &Path, e: std::io::Error) -> ScionError {
    ScionError::new(
        ErrorCode::Unavailable,
        format!("daemon: {what} {}: {e}", path.display()),
    )
}

/// Remove a stale socket left by a crashed daemon; refuse to delete anything else.
fn remove_stale_socket(path: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => {
            std::fs::remove_file(path).map_err(|e| io_error("remove stale socket", path, e))
        }
        Ok(_) => Err(ScionError::new(
            ErrorCode::InvalidArgument,
            format!(
                "daemon: socket path {} exists and is not a socket",
                path.display()
            ),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error("stat", path, e)),
    }
}

/// Refuse to start when another process is accepting connections on `path`: deleting a live
/// daemon's socket would silently take over its clients.
fn refuse_live_socket(path: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let is_socket = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket());
    if is_socket && std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(ScionError::new(
            ErrorCode::Unavailable,
            format!(
                "daemon: socket {} is in use by a running process",
                path.display()
            ),
        ));
    }
    Ok(())
}

impl Daemon {
    /// Open the verified cache, bind the Unix socket (mode 0666: authorization rests on
    /// per-request tokens, never on socket identity, SPEC 17) and start serving: the local
    /// ObjectService (SPEC 7.3), verbatim forwarding of everything else (SPEC 7.8), the
    /// redaction poll loop (SPEC 13.3) and epoch trailers with tier `daemon` (SPEC 7.1).
    ///
    /// The upstream need not be reachable at start; it is dialed on first use.
    pub async fn start(
        cfg: DaemonConfig,
        upstream: Arc<dyn UpstreamConnector>,
        clock: Arc<dyn Clock>,
        metrics: Arc<Metrics>,
    ) -> Result<DaemonHandle> {
        // Before touching the cache: opening it deletes temp files, which would break a live
        // daemon's in-progress inserts.
        refuse_live_socket(&cfg.socket)?;
        let (dir, max, low, verify) = (
            cfg.cache_dir.clone(),
            cfg.max_cache_bytes,
            cfg.low_watermark_ratio,
            cfg.verify_on_read,
        );
        let cache = tokio::task::spawn_blocking(move || DiskCache::open(&dir, max, low, verify))
            .await
            .map_err(|e| {
                ScionError::new(ErrorCode::Unavailable, format!("cache open task: {e}"))
            })??;
        let cache = Arc::new(cache);

        let socket = cfg.socket.clone();
        if let Some(parent) = socket.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| io_error("create", parent, e))?;
        }
        remove_stale_socket(&socket)?;
        let listener =
            tokio::net::UnixListener::bind(&socket).map_err(|e| io_error("bind", &socket, e))?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666))
                .map_err(|e| io_error("chmod", &socket, e))?;
        }

        let inner = Arc::new(Inner::new(
            cfg,
            upstream.clone(),
            clock.clone(),
            metrics.clone(),
            cache.clone(),
        ));
        inner.sync_cache_metrics();
        inner.update_redaction_metrics();

        let dispatch = forward::Dispatch::new(inner.clone());
        let layer = EpochTrailerLayer::new(Arc::new(DaemonEpochs(inner.clone())));
        let shutdown = inner.shutdown.clone();
        let server = tokio::spawn(async move {
            let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
            let res = tonic::transport::Server::builder()
                .layer(layer)
                .serve_with_incoming_shutdown(dispatch, incoming, shutdown.cancelled_owned())
                .await;
            if let Err(e) = res {
                tracing::error!(error = %e, "daemon server stopped");
            }
        });
        let poller = tokio::spawn(inner.clone().redaction_loop());
        tracing::info!(socket = %socket.display(), "scion-daemon listening");
        Ok(DaemonHandle {
            socket,
            cache,
            inner,
            upstream,
            clock,
            metrics,
            tasks: parking_lot::Mutex::new(Some((server, poller))),
        })
    }
}

/// A running daemon (SPEC 10).
pub struct DaemonHandle {
    socket: PathBuf,
    cache: Arc<DiskCache>,
    inner: Arc<Inner>,
    upstream: Arc<dyn UpstreamConnector>,
    clock: Arc<dyn Clock>,
    metrics: Arc<Metrics>,
    tasks: parking_lot::Mutex<Option<(tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>)>>,
}

impl DaemonHandle {
    /// The Unix socket path (SPEC 6.3 `socket`).
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// `unix://` endpoint of the socket (SPEC 6.4).
    pub fn endpoint(&self) -> Endpoint {
        Endpoint::Unix(self.socket.clone())
    }

    /// Current counters and state (SPEC 10, 13.3).
    pub fn stats(&self) -> DaemonStats {
        let i = &self.inner;
        DaemonStats {
            upstream_requests: i.upstream_requests.load(Ordering::Relaxed),
            cache_hits: i.cache_hits.load(Ordering::Relaxed),
            cache_misses: i.cache_misses.load(Ordering::Relaxed),
            fail_closed: i.fail_closed_now(),
            redaction_epoch: i.redactions.epoch(),
            policy_epoch: i.policy_epoch().unwrap_or(0),
        }
    }

    /// The object cache (SPEC 10.2).
    pub fn cache(&self) -> &DiskCache {
        &self.cache
    }

    /// Test hook: pause redaction polling (SPEC 19.13 fail-closed test). While paused no
    /// refresh succeeds, so the set goes stale after `redaction_max_staleness_ms`.
    pub fn block_redaction_polling(&self, on: bool) {
        self.inner.poll_blocked.store(on, Ordering::Release);
        if !on {
            self.inner.poll_now.notify_one();
        }
    }

    /// The upstream connector this daemon uses.
    pub fn upstream(&self) -> Arc<dyn UpstreamConnector> {
        self.upstream.clone()
    }

    /// The clock this daemon uses.
    pub fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }

    /// The metrics registry this daemon reports into (SPEC 15.2).
    pub fn metrics(&self) -> Arc<Metrics> {
        self.metrics.clone()
    }

    /// Stop serving (in-flight calls get a grace period), stop polling and remove the
    /// socket. Idempotent; the cache directory stays valid (SPEC 16.1 class 6).
    pub(crate) async fn stop(&self) {
        self.inner.shutdown.cancel();
        let tasks = self.tasks.lock().take();
        if let Some((server, poller)) = tasks {
            poller.abort();
            let abort = server.abort_handle();
            if tokio::time::timeout(SHUTDOWN_GRACE, server).await.is_err() {
                abort.abort();
            }
        }
        if let Err(e) = remove_stale_socket(&self.socket) {
            tracing::warn!(error = %e, "could not remove the daemon socket");
        }
    }

    /// Stop serving and remove the socket.
    pub async fn shutdown(self) {
        self.stop().await;
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        if let Some((server, poller)) = self.tasks.lock().take() {
            self.inner.shutdown.cancel();
            server.abort();
            poller.abort();
        }
    }
}
