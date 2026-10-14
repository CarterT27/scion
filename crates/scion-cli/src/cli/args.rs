//! Command-line grammar (SPEC 11.8; PLAN 2.18) with clap derive.

use clap::{ArgGroup, Args, Parser, Subcommand};
use std::path::PathBuf;

/// The `--help` epilogue (PLAN 6.15).
pub(crate) const EPILOGUE: &str = "\
Revisions (SPEC 11.8): a changeset id or unique prefix (>= 8 hex), a ref name, <ref>@<version>,
git:<sha>, with suffixes ~<n> (n-th first-parent ancestor) and ^<k> (k-th parent).

Environment: SCION_ENDPOINT, SCION_TOKEN (overrides the credentials file), SCION_CA_FILE,
SCION_AUTHOR (commit author), SCION_SERVICE_SECRET (login --service), SCION_LOG (log filter).
Endpoint precedence: --endpoint > SCION_ENDPOINT > <root>/.scion/config.toml >
~/.config/scion/config.toml. Credentials: ~/.config/scion/credentials (mode 0600).

--json prints one line {\"schema\":\"scion.cli.<cmd>.v1\",\"data\":...} (schemas/v1/); errors
print the scion.cli.error.v1 envelope.

Exit codes:
  0  success
  1  error
  2  usage error
  3  conflicts (rebase paused on conflicts, unresolved conflicts)
  4  refused because of dirty files (use --force where offered)";

/// `scion`: the Scion command-line client (SPEC 11.8).
#[derive(Parser, Debug)]
#[command(
    name = "scion",
    version,
    about = "Scion source control client (SPEC 11.8)",
    after_help = EPILOGUE,
    propagate_version = true
)]
pub(crate) struct Cli {
    #[command(flatten)]
    pub global: Global,
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// Options accepted by every command.
#[derive(Args, Debug, Clone, Default)]
pub(crate) struct Global {
    /// Print a versioned JSON envelope instead of human output.
    #[arg(long, global = true)]
    pub json: bool,
    /// Server or daemon endpoint: unix:///path or https://host:port.
    #[arg(long, global = true, value_name = "URL")]
    pub endpoint: Option<String>,
    /// PEM file of the CA to trust for HTTPS (else SCION_CA_FILE, else the native roots).
    #[arg(long, global = true, value_name = "PEM")]
    pub ca_file: Option<PathBuf>,
    /// Run as if started in DIR.
    #[arg(short = 'C', global = true, value_name = "DIR")]
    pub dir: Option<PathBuf>,
}

/// Core commands (SPEC 11.8).
#[derive(Subcommand, Debug)]
pub(crate) enum Cmd {
    /// Log in with the GitHub device flow (or as a service principal).
    Login(LoginArgs),
    /// Show the authenticated principal.
    Whoami,
    /// Check out a revision into a new working copy.
    Checkout(CheckoutArgs),
    /// Change the sparse profile.
    Sparse {
        #[command(subcommand)]
        action: SparseCmd,
    },
    /// Show working-copy changes.
    Status,
    /// Show working-copy changes against the base as unified diffs.
    Diff(DiffArgs),
    /// Commit every change (or only --path) on top of the base.
    Commit(CommitArgs),
    /// Amend the base changeset (same change id).
    Amend(AmendArgs),
    /// Move the working copy to another revision.
    Goto(GotoArgs),
    /// Rebase the local stack onto a revision, or abort a paused rebase.
    Rebase(RebaseArgs),
    /// Mark a conflicted path resolved.
    Resolve(ResolveArgs),
    /// Land the base onto a ref.
    Land(LandArgs),
    /// Show history.
    Log(LogArgs),
    /// Show line origins of a file.
    Blame(BlameArgs),
    /// Show a changeset.
    Show(ShowArgs),
    /// Print a file at a revision: <rev>:<path>.
    Cat(CatArgs),
    /// Redact objects (admin).
    Redact(RedactArgs),
}

impl Cmd {
    /// The command name used in the `--json` schema id.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Cmd::Login(_) => "login",
            Cmd::Whoami => "whoami",
            Cmd::Checkout(_) => "checkout",
            Cmd::Sparse { .. } => "sparse",
            Cmd::Status => "status",
            Cmd::Diff(_) => "diff",
            Cmd::Commit(_) => "commit",
            Cmd::Amend(_) => "amend",
            Cmd::Goto(_) => "goto",
            Cmd::Rebase(_) => "rebase",
            Cmd::Resolve(_) => "resolve",
            Cmd::Land(_) => "land",
            Cmd::Log(_) => "log",
            Cmd::Blame(_) => "blame",
            Cmd::Show(_) => "show",
            Cmd::Cat(_) => "cat",
            Cmd::Redact(_) => "redact",
        }
    }
}

#[derive(Args, Debug)]
pub(crate) struct LoginArgs {
    /// Log in as service principal NAME; the secret comes from SCION_SERVICE_SECRET or the
    /// first line of standard input.
    #[arg(long, value_name = "NAME")]
    pub service: Option<String>,
    /// Do not store the GitHub token for silent re-exchange.
    #[arg(long)]
    pub no_store_github_token: bool,
}

#[derive(Args, Debug)]
pub(crate) struct DiffArgs {
    /// List changed files only (no content; no server contact when every tree is cached).
    #[arg(long)]
    pub name_only: bool,
    /// Lines of context around each hunk.
    #[arg(
        short = 'U',
        long = "unified",
        value_name = "N",
        default_value_t = 3,
        conflicts_with = "name_only"
    )]
    pub unified: usize,
}

#[derive(Args, Debug)]
pub(crate) struct CheckoutArgs {
    /// Revision to check out.
    pub rev: String,
    /// Target directory (default: the current directory).
    pub dest: Option<PathBuf>,
    /// Sparse profile prefixes (default: the whole tree).
    #[arg(long, num_args = 1.., value_name = "PATH")]
    pub sparse: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum SparseCmd {
    /// Add prefixes and materialize their files.
    Add {
        /// Prefixes.
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Remove prefixes and their files.
    Remove {
        /// Prefixes.
        #[arg(required = true)]
        paths: Vec<String>,
        /// Remove even if files are dirty.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Args, Debug)]
pub(crate) struct CommitArgs {
    /// Message (required, except for the commit that continues a paused rebase, which keeps
    /// the original message, SPEC 11.7).
    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,
    /// Restrict the commit to these paths.
    #[arg(long = "path", value_name = "PATH")]
    pub paths: Vec<String>,
}

#[derive(Args, Debug)]
pub(crate) struct AmendArgs {
    /// New message (default: keep the original Meta).
    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct GotoArgs {
    /// Destination revision.
    pub rev: String,
    /// Overwrite dirty files the move touches.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
#[command(group(ArgGroup::new("mode").required(true).args(["onto", "abort"])))]
pub(crate) struct RebaseArgs {
    /// Destination revision.
    #[arg(long, value_name = "REV")]
    pub onto: Option<String>,
    /// Abort a paused rebase and return to the original changeset.
    #[arg(long)]
    pub abort: bool,
}

#[derive(Args, Debug)]
pub(crate) struct ResolveArgs {
    /// Conflicted path.
    pub path: String,
    /// Resolve even if conflict markers remain.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub(crate) struct LandArgs {
    /// Ref to land on (default: the tracked ref, else main).
    #[arg(long = "ref", value_name = "REF")]
    pub ref_name: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct LogArgs {
    /// Only changesets that changed this path.
    pub path: Option<String>,
    /// Starting revision (default: the working-copy base, else main).
    #[arg(long, value_name = "REV")]
    pub rev: Option<String>,
    /// Follow first parents only.
    #[arg(long)]
    pub first_parent: bool,
    /// At most N entries.
    #[arg(long, value_name = "N")]
    pub limit: Option<u32>,
    /// One line per changeset: short id and first message line.
    #[arg(long)]
    pub oneline: bool,
}

#[derive(Args, Debug)]
pub(crate) struct BlameArgs {
    /// File path.
    pub path: String,
    /// Revision (default: the working-copy base, else main).
    #[arg(long, value_name = "REV")]
    pub rev: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct ShowArgs {
    /// Revision (default: the working-copy base, else main).
    pub rev: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct CatArgs {
    /// <rev>:<path>.
    pub spec: String,
}

#[derive(Args, Debug)]
#[command(group(ArgGroup::new("target").required(true).args(["path", "object", "meta"])))]
pub(crate) struct RedactArgs {
    /// Redact the blob at PATH (with --at).
    #[arg(long, value_name = "PATH", requires = "at")]
    pub path: Option<String>,
    /// Revision for --path.
    #[arg(long, value_name = "REV", requires = "path")]
    pub at: Option<String>,
    /// With --path: every distinct blob the path ever held in history reachable from --at.
    #[arg(long, requires = "path")]
    pub all_versions: bool,
    /// Redact this object id (blob or meta).
    #[arg(long, value_name = "ID")]
    pub object: Option<String>,
    /// Redact the Meta of this revision.
    #[arg(long, value_name = "REV")]
    pub meta: Option<String>,
    /// Audit reason.
    #[arg(long)]
    pub reason: Option<String>,
    /// Quarantine period before the scheduled purge (e.g. 7d; default: the server's).
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    pub purge_after: Option<std::time::Duration>,
    /// Purge irreversibly right away.
    #[arg(long)]
    pub purge_now: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn grammar_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn rebase_and_redact_groups() {
        assert!(Cli::try_parse_from(["scion", "rebase"]).is_err());
        assert!(Cli::try_parse_from(["scion", "rebase", "--onto", "main", "--abort"]).is_err());
        assert!(Cli::try_parse_from(["scion", "rebase", "--abort"]).is_ok());
        assert!(Cli::try_parse_from(["scion", "redact"]).is_err());
        assert!(Cli::try_parse_from(["scion", "redact", "--path", "a"]).is_err());
        assert!(Cli::try_parse_from(["scion", "redact", "--path", "a", "--at", "main"]).is_ok());
        assert!(Cli::try_parse_from(["scion", "redact", "--object", "x", "--meta", "y"]).is_err());
        assert!(
            Cli::try_parse_from(["scion", "redact", "--all-versions", "--object", "x"]).is_err()
        );
        let c = Cli::try_parse_from(["scion", "--json", "status"]).unwrap();
        assert!(c.global.json);
        let c = Cli::try_parse_from(["scion", "status", "--json", "-C", "/tmp"]).unwrap();
        assert!(c.global.json);
        assert_eq!(c.global.dir.as_deref(), Some(std::path::Path::new("/tmp")));
    }

    #[test]
    fn help_documents_exit_codes() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("Exit codes:"), "{help}");
        assert!(help.contains("4  refused because of dirty files"), "{help}");
    }
}
