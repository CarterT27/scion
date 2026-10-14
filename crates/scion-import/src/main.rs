//! `scion-import` binary (SPEC 14; implementation-defined 6.4; P29).
//!
//! ```text
//! scion-import --git <repo> --ref <git-ref> --target-ref main [--skip-submodules]
//!              [--endpoint <unix:///path | https://host:port>] [--ca-file <pem>]
//!              [--batch-bytes <n>]
//! ```
//!
//! Authentication: `SCION_TOKEN`, or a service login with `SCION_SERVICE_NAME` and
//! `SCION_SERVICE_SECRET`. The endpoint may also come from `SCION_ENDPOINT` and the CA from
//! `SCION_CA_FILE`. On success the [`scion_import::ImportReport`] is printed as JSON on
//! stdout; on failure the error goes to stderr and the exit status is 1.

use anyhow::{Context, bail};
use clap::Parser;
use scion_import::{ImportOptions, import};
use scion_object::RefName;
use scion_proto::{Endpoint, ScionClient, TlsOptions, error_from_status, v1};
use std::path::PathBuf;

/// Default upload batch size: 8 MiB of blob content per UploadObjects call.
const DEFAULT_BATCH_BYTES: usize = 8 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "scion-import",
    about = "One-way import of a git history into Scion (SPEC 14)"
)]
struct Args {
    /// The git repository (bare or with a work tree).
    #[arg(long = "git", value_name = "REPO")]
    git: PathBuf,
    /// The git ref (or any revision) to import.
    #[arg(long = "ref", value_name = "GIT_REF")]
    git_ref: String,
    /// The Scion ref to create or fast-forward.
    #[arg(long = "target-ref", value_name = "REF", default_value = "main")]
    target_ref: String,
    /// Omit gitlinks (recorded in Meta extension tag 4) instead of failing.
    #[arg(long)]
    skip_submodules: bool,
    /// Server endpoint: unix:///path or https://host:port.
    #[arg(long, env = "SCION_ENDPOINT")]
    endpoint: String,
    /// PEM file of the CA to trust for HTTPS (default: the native roots).
    #[arg(long, env = "SCION_CA_FILE")]
    ca_file: Option<PathBuf>,
    /// Upload batch size in bytes.
    #[arg(long, default_value_t = DEFAULT_BATCH_BYTES)]
    batch_bytes: usize,
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

async fn connect(args: &Args) -> anyhow::Result<ScionClient> {
    let ep = Endpoint::parse(&args.endpoint)?;
    let tls = match &args.ca_file {
        Some(p) => Some(TlsOptions {
            ca_pem: Some(std::fs::read(p).with_context(|| format!("read {}", p.display()))?),
            domain: None,
        }),
        None => None,
    };
    if let Some(token) = env_nonempty("SCION_TOKEN") {
        return Ok(ScionClient::connect(&ep, tls, Some(token)).await?);
    }
    let (Some(name), Some(secret)) = (
        env_nonempty("SCION_SERVICE_NAME"),
        env_nonempty("SCION_SERVICE_SECRET"),
    ) else {
        bail!("set SCION_TOKEN, or SCION_SERVICE_NAME and SCION_SERVICE_SECRET");
    };
    let anon = ScionClient::connect(&ep, tls, None).await?;
    let resp = anon
        .auth()
        .login(v1::LoginRequest {
            credential: Some(v1::login_request::Credential::Service(
                v1::ServiceCredential {
                    service_name: name,
                    service_secret: secret,
                },
            )),
        })
        .await
        .map_err(|s| error_from_status(&s))
        .context("service login")?
        .into_inner();
    Ok(anon.with_token(Some(resp.token)))
}

async fn run(args: Args) -> anyhow::Result<()> {
    let target_ref = RefName::parse(&args.target_ref)?;
    let client = connect(&args).await?;
    let opts = ImportOptions {
        git_dir: args.git.clone(),
        git_ref: args.git_ref.clone(),
        target_ref,
        skip_submodules: args.skip_submodules,
        batch_bytes: args.batch_bytes,
    };
    let report = import(&client, &opts).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("scion-import: cannot start runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(args)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("scion-import: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
