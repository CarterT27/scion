//! `scion-server` binary: load the config, connect CockroachDB and S3, serve (SPEC 3.1, 6.1;
//! P23).
//!
//! Usage: `scion-server [--config] <scion-server.toml>` (default `/etc/scion/scion-server.toml`,
//! or `$SCION_SERVER_CONFIG`). Startup fails on an invalid config, an invalid or missing policy
//! file, unreadable signing keys or TLS material, or unreachable backends (SPEC 6, 6.2).

use anyhow::Context as _;
use scion_config::{ProcessEnv, load_server};
use scion_core::{FaultRegistry, SystemClock};
use scion_server::{ServerApp, ServerDeps};
use std::path::PathBuf;
use std::sync::Arc;

const DEFAULT_CONFIG: &str = "/etc/scion/scion-server.toml";

fn config_path() -> anyhow::Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    match (args.next(), args.next()) {
        (None, _) => Ok(std::env::var_os("SCION_SERVER_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG))),
        (Some(flag), Some(p)) if flag == "--config" || flag == "-c" => Ok(PathBuf::from(p)),
        (Some(flag), None) if flag == "--help" || flag == "-h" => {
            anyhow::bail!("usage: scion-server [--config] <scion-server.toml>")
        }
        (Some(p), None) if !p.starts_with('-') => Ok(PathBuf::from(p)),
        _ => anyhow::bail!("usage: scion-server [--config] <scion-server.toml>"),
    }
}

fn main() -> anyhow::Result<()> {
    let path = config_path()?;
    let config =
        load_server(&path, &ProcessEnv).with_context(|| format!("loading {}", path.display()))?;
    let format = if config.observability.log_format == "text" {
        scion_obs::LogFormat::Text
    } else {
        scion_obs::LogFormat::Json
    };
    let _logs = scion_obs::init_logging(format);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the tokio runtime")?
        .block_on(run(config))
}

async fn run(config: scion_config::ServerConfig) -> anyhow::Result<()> {
    let policy = scion_policy::PolicyHandle::load(&config.policy.path)
        .with_context(|| format!("loading policy {}", config.policy.path.display()))?;
    let keyring = scion_auth::Keyring::from_files(&config.auth.signing_keys)
        .context("loading signing keys")?;
    let (cert, key) = scion_server_tls(&config)?;

    let meta = scion_store_crdb::CrdbMetaStore::connect(scion_store_crdb::CrdbConfig {
        url: config.metadata.url.clone(),
        max_connections: config.metadata.max_connections,
        repo_name: config.server.repo.clone(),
        tls: None,
    })
    .await
    .context("connecting to the metadata store")?;
    meta.migrate()
        .await
        .context("migrating the metadata store")?;
    let blobs = scion_store_s3::S3BlobStore::new(scion_store_s3::S3Config {
        endpoint: config.objects.endpoint.clone(),
        bucket: config.objects.bucket.clone(),
        region: config.objects.region.clone(),
        access_key: None,
        secret_key: None,
        allow_http: config.objects.endpoint.starts_with("http://"),
        path_style: config.objects.path_style,
        key_prefix: String::new(),
    })
    .context("configuring the object store")?;
    let storage = scion_store::Storage::new(Arc::new(meta), Arc::new(blobs), FaultRegistry::new());

    let listen = config.server.listen;
    let app = ServerApp::build(ServerDeps {
        storage,
        github: scion_auth::GitHubClient::new(&config.auth.github_api_url),
        config,
        policy,
        keyring,
        clock: Arc::new(SystemClock),
        metrics: scion_obs::Metrics::new(),
        registry: None,
    })
    .await
    .context("building the server")?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let handle = app
        .serve(listener, Some((cert, key)))
        .await
        .context("serving")?;
    tracing::info!(addr = %handle.addr, admin = ?handle.admin_addr, "scion-server started");
    shutdown_signal().await;
    tracing::info!("shutting down");
    handle.shutdown().await;
    Ok(())
}

fn scion_server_tls(config: &scion_config::ServerConfig) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let read = |p: &std::path::Path| {
        std::fs::read(p).with_context(|| format!("reading TLS file {}", p.display()))
    };
    Ok((
        read(&config.server.tls_cert)?,
        read(&config.server.tls_key)?,
    ))
}

async fn shutdown_signal() {
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
