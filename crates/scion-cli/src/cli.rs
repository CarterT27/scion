//! Command dispatch (SPEC 11.8; PLAN 2.18, 6.15; P31).
//!
//! [`run`] parses the arguments with clap, runs the command on a single-threaded tokio
//! runtime and prints either human output or the `--json` envelope. An UNAUTHENTICATED
//! failure with credentials that hold a GitHub token triggers one silent re-exchange and a
//! retry (SPEC 12.2).

use std::ffi::OsString;
use std::io::Write;

mod args;
mod auth;
mod ctx;
mod error;
mod query;
mod redact;
mod util;
mod wc_cmds;

use args::{Cli, Cmd};
use clap::Parser;
use ctx::{Ctx, ENV_LOG, Reply};
use error::{CliError, CliResult, EXIT_ERROR, EXIT_USAGE};

/// Process IO abstraction so commands are testable in-process.
pub trait CliIo {
    /// Standard output.
    fn stdout(&mut self) -> &mut dyn Write;
    /// Standard error.
    fn stderr(&mut self) -> &mut dyn Write;
    /// One line from standard input, if any.
    fn stdin_line(&mut self) -> Option<String>;
}

/// Run the CLI with `args` (including `argv[0]`); returns the exit code: 0 ok, 1 error,
/// 2 usage, 3 conflicts, 4 dirty refusal (PLAN 6.15).
pub fn run(args: Vec<OsString>, env: &dyn scion_config::Env, io: &mut dyn CliIo) -> i32 {
    let json_requested = args.iter().skip(1).any(|a| a == "--json");
    let cli = match Cli::try_parse_from(&args) {
        Ok(c) => c,
        Err(e) => {
            let code = e.exit_code();
            if code == 0 {
                // --help / --version.
                let _ = write!(io.stdout(), "{e}");
                return 0;
            }
            if json_requested {
                let err = CliError::Usage(e.kind().to_string());
                print_error(io, true, &err, &e.to_string());
            } else {
                let _ = write!(io.stderr(), "{e}");
            }
            return EXIT_USAGE;
        }
    };
    let json = cli.global.json;
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let err = error::local(format!("cannot start the async runtime: {e}"));
            print_error(io, json, &err, &err.human());
            return EXIT_ERROR;
        }
    };
    let _log = install_logging(env);
    let result = rt.block_on(execute(&cli, env, io));
    match result {
        Ok(reply) => {
            emit(io, json, &reply);
            reply.exit
        }
        Err(err) => {
            print_error(io, json, &err, &err.human());
            err.exit_code()
        }
    }
}

/// A thread-local stderr subscriber with the `SCION_LOG` filter (default `warn`).
fn install_logging(env: &dyn scion_config::Env) -> tracing::subscriber::DefaultGuard {
    let filter = env
        .var(ENV_LOG)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "warn".to_owned());
    let filter = tracing_subscriber::EnvFilter::try_new(&filter)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let sub = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .finish();
    tracing::subscriber::set_default(sub)
}

/// Run the parsed command, retrying once after a silent token re-exchange (SPEC 12.2).
async fn execute(cli: &Cli, env: &dyn scion_config::Env, io: &mut dyn CliIo) -> CliResult<Reply> {
    tracing::debug!(command = cli.cmd.name(), "running");
    let mut ctx = Ctx::new(env, cli.global.clone())?;
    match dispatch(&cli.cmd, &mut ctx, io).await {
        Err(CliError::Scion(e))
            if e.code == scion_core::ErrorCode::Unauthenticated
                && !matches!(cli.cmd, Cmd::Login(_))
                && ctx.can_reexchange() =>
        {
            tracing::info!("token rejected; re-exchanging the stored GitHub token");
            let root = ctx.find_root();
            ctx.reexchange(root.as_deref()).await?;
            dispatch(&cli.cmd, &mut ctx, io).await
        }
        other => other,
    }
}

async fn dispatch(cmd: &Cmd, ctx: &mut Ctx<'_>, io: &mut dyn CliIo) -> CliResult<Reply> {
    match cmd {
        Cmd::Login(a) => auth::login(ctx, io, a).await,
        Cmd::Whoami => auth::whoami(ctx).await,
        Cmd::Checkout(a) => wc_cmds::checkout(ctx, a).await,
        Cmd::Sparse { action } => wc_cmds::sparse(ctx, action).await,
        Cmd::Status => wc_cmds::status(ctx).await,
        Cmd::Diff(a) => wc_cmds::diff(ctx, a).await,
        Cmd::Commit(a) => wc_cmds::commit(ctx, a).await,
        Cmd::Amend(a) => wc_cmds::amend(ctx, a).await,
        Cmd::Goto(a) => wc_cmds::goto(ctx, a).await,
        Cmd::Rebase(a) => wc_cmds::rebase(ctx, a).await,
        Cmd::Resolve(a) => wc_cmds::resolve(ctx, a).await,
        Cmd::Land(a) => wc_cmds::land(ctx, a).await,
        Cmd::Log(a) => query::log(ctx, a).await,
        Cmd::Blame(a) => query::blame(ctx, a).await,
        Cmd::Show(a) => query::show(ctx, a).await,
        Cmd::Cat(a) => query::cat(ctx, a).await,
        Cmd::Redact(a) => redact::redact(ctx, a).await,
    }
}

/// Print a reply: the envelope as one JSON line, or the human output.
fn emit(io: &mut dyn CliIo, json: bool, reply: &Reply) {
    let out = io.stdout();
    if json {
        let line = serde_json::to_string(&reply.json).unwrap_or_else(|_| "{}".to_owned());
        let _ = writeln!(out, "{line}");
    } else {
        let _ = out.write_all(&reply.human);
    }
    let _ = out.flush();
}

/// Print an error: the `scion.cli.error.v1` envelope on stdout under `--json`, else
/// `scion: error: ...` on stderr.
fn print_error(io: &mut dyn CliIo, json: bool, err: &CliError, human: &str) {
    if json {
        let (code, subcode) = err.code();
        let data = crate::output::views::ErrorData {
            code,
            subcode,
            message: match err {
                CliError::Usage(_) => human.trim_end().to_owned(),
                _ => err.message(),
            },
            exit_code: err.exit_code(),
        };
        let env = crate::output::envelope(crate::output::ERROR_SCHEMA, data);
        let line = serde_json::to_string(&env).unwrap_or_else(|_| "{}".to_owned());
        let out = io.stdout();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    } else {
        let e = io.stderr();
        let _ = writeln!(e, "scion: error: {human}");
        let _ = e.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_config::MapEnv;

    #[derive(Default)]
    struct BufIo {
        out: Vec<u8>,
        err: Vec<u8>,
    }

    impl CliIo for BufIo {
        fn stdout(&mut self) -> &mut dyn Write {
            &mut self.out
        }
        fn stderr(&mut self) -> &mut dyn Write {
            &mut self.err
        }
        fn stdin_line(&mut self) -> Option<String> {
            None
        }
    }

    fn run_args(args: &[&str], env: &MapEnv) -> (i32, String, String) {
        let mut io = BufIo::default();
        let code = run(args.iter().map(OsString::from).collect(), env, &mut io);
        (
            code,
            String::from_utf8_lossy(&io.out).into_owned(),
            String::from_utf8_lossy(&io.err).into_owned(),
        )
    }

    #[test]
    fn help_and_usage_exit_codes() {
        let env = MapEnv::default();
        let (code, out, _) = run_args(&["scion", "--help"], &env);
        assert_eq!(code, 0);
        assert!(out.contains("Exit codes:"));
        let (code, _, err) = run_args(&["scion", "frobnicate"], &env);
        assert_eq!(code, 2);
        assert!(!err.is_empty());
        let (code, out, _) = run_args(&["scion", "--json", "rebase"], &env);
        assert_eq!(code, 2);
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["schema"], "scion.cli.error.v1");
        assert_eq!(v["data"]["code"], "USAGE");
        assert_eq!(v["data"]["exit_code"], 2);
    }

    #[test]
    fn no_endpoint_is_an_error_not_usage() {
        let home = tempfile::tempdir().unwrap();
        let env = MapEnv::from_pairs([("HOME", home.path().to_str().unwrap())]);
        let (code, _, err) = run_args(&["scion", "whoami"], &env);
        assert_eq!(code, 1);
        assert!(err.contains("no endpoint configured"), "{err}");
        let (code, out, _) = run_args(&["scion", "--json", "whoami"], &env);
        assert_eq!(code, 1);
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["data"]["code"], "LOCAL");
        let schema = crate::output::schema_for("error").unwrap();
        assert!(jsonschema::is_valid(&schema, &v));
    }

    #[test]
    fn outside_a_working_copy() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let env = MapEnv::from_pairs([
            ("HOME", home.path().to_str().unwrap()),
            ("SCION_ENDPOINT", "unix:///nonexistent/scion.sock"),
        ]);
        let (code, _, err) = run_args(
            &["scion", "-C", dir.path().to_str().unwrap(), "status"],
            &env,
        );
        assert_eq!(code, 1);
        assert!(err.contains("not inside a scion working copy"), "{err}");
    }
}
