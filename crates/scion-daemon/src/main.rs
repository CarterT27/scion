//! `scion-daemon` binary: load the daemon config, then start (SPEC 6.3, 10; P16).
//!
//! Usage: `scion-daemon [--config <path>]` (default `/etc/scion/scion-daemon.toml`, or the
//! `SCION_DAEMON_CONFIG` environment variable). Stops on SIGINT or SIGTERM.

use anyhow::Context as _;
use scion_config::{ProcessEnv, load_daemon};
use scion_core::SystemClock;
use scion_daemon::{Daemon, DirectUpstream};
use scion_obs::{LogFormat, Metrics, init_logging};
use scion_proto::{Endpoint, TlsOptions};
use std::path::PathBuf;
use std::sync::Arc;

const DEFAULT_CONFIG: &str = "/etc/scion/scion-daemon.toml";

fn config_path() -> anyhow::Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    let mut path = std::env::var_os("SCION_DAEMON_CONFIG").map(PathBuf::from);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" | "-c" => {
                path = Some(PathBuf::from(args.next().context("--config needs a path")?));
            }
            "-h" | "--help" => {
                println!("usage: scion-daemon [--config <path>]");
                std::process::exit(0);
            }
            other => anyhow::bail!(
                "unexpected argument {other:?} (usage: scion-daemon [--config <path>])"
            ),
        }
    }
    Ok(path.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG)))
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn main() -> anyhow::Result<()> {
    let path = config_path()?;
    let cfg =
        load_daemon(&path, &ProcessEnv).with_context(|| format!("loading {}", path.display()))?;
    let _log = init_logging(LogFormat::Json);
    let upstream = Endpoint::parse(&cfg.upstream).context("parsing upstream")?;
    let tls = match &cfg.upstream_ca {
        Some(ca) => Some(TlsOptions {
            ca_pem: Some(
                std::fs::read(ca)
                    .with_context(|| format!("reading upstream_ca {}", ca.display()))?,
            ),
            domain: None,
        }),
        None => None,
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")?;
    rt.block_on(async move {
        let connector = Arc::new(DirectUpstream::new(upstream, tls));
        let handle = Daemon::start(cfg, connector, Arc::new(SystemClock), Metrics::new())
            .await
            .context("starting the daemon")?;
        wait_for_signal().await;
        handle.shutdown().await;
        Ok(())
    })
}
