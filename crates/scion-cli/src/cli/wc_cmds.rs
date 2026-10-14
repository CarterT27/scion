//! Working-copy commands: checkout, sparse, status, diff, commit, amend, goto, rebase, resolve,
//! land (SPEC 11.2-11.7).

use super::args::{
    AmendArgs, CheckoutArgs, CommitArgs, GotoArgs, LandArgs, RebaseArgs, ResolveArgs, SparseCmd,
};
use super::ctx::{Ctx, ENV_AUTHOR, Reply, write_wc_config};
use super::error::{CliResult, EXIT_CONFLICTS, usage};
use super::util::{lossy, parse_file_path, parse_path, path_text, short};
use crate::output::views::{
    CheckoutData, CollisionView, CommitData, DeltaView, DiffData, RebaseData, ResolveData,
    SparseData, StatusData,
};
use scion_config::WcConfig;
use scion_object::{DeltaOp, RefName, RepoPath};
use scion_proto::{error_from_status, v1};
use scion_wc::state::{PendingOperation, WcState};
use scion_wc::{CommitReport, WcOptions, WorkingCopy};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Open the working copy containing the current directory.
async fn open(ctx: &mut Ctx<'_>) -> CliResult<WorkingCopy> {
    let root = ctx.require_root()?;
    let client = ctx.client(Some(&root)).await?;
    Ok(WorkingCopy::open(&root, client, WcOptions::default())?)
}

/// [`open`] with a lazily connected client: no server contact unless the command issues an
/// RPC (SPEC 11.5: `status`/`diff` work offline when every needed tree is cached).
fn open_lazy(ctx: &mut Ctx<'_>) -> CliResult<WorkingCopy> {
    let root = ctx.require_root()?;
    let client = ctx.lazy_client(Some(&root))?;
    Ok(WorkingCopy::open(&root, client, WcOptions::default())?)
}

fn paths_text(v: &[RepoPath]) -> Vec<String> {
    v.iter().map(path_text).collect()
}

/// Kind name of a pending operation.
fn pending_kind(p: &PendingOperation) -> &'static str {
    match p {
        PendingOperation::Checkout { .. } => "checkout",
        PendingOperation::Goto { .. } => "goto",
        PendingOperation::SparseChange { .. } => "sparse",
        PendingOperation::Commit { .. } => "commit",
        PendingOperation::Rebase { .. } => "rebase",
    }
}

/// Unresolved conflict paths of a paused rebase (SPEC 11.7).
fn unresolved(st: &WcState) -> Vec<String> {
    match &st.pending {
        Some(PendingOperation::Rebase {
            conflicts,
            resolved,
            ..
        }) => conflicts
            .iter()
            .filter(|c| !resolved.contains(c))
            .map(|c| lossy(c))
            .collect(),
        _ => Vec::new(),
    }
}

/// `c1abc… (main)` rendering of the base.
fn base_label(st: &WcState) -> String {
    match &st.tracking_ref {
        Some(r) => format!("{} ({})", short(&st.base), r.as_str()),
        None => short(&st.base),
    }
}

fn absolute(cwd: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// `scion checkout <rev> [DIR] [--sparse p...]` (SPEC 11.3).
pub(crate) async fn checkout(ctx: &mut Ctx<'_>, a: &CheckoutArgs) -> CliResult<Reply> {
    let dest = match &a.dest {
        Some(d) => absolute(ctx.cwd(), d),
        None => ctx.cwd().to_path_buf(),
    };
    let sparse = a
        .sparse
        .iter()
        .map(|s| parse_path(s))
        .collect::<CliResult<Vec<_>>>()?;
    let (client, endpoint) = ctx.client_and_endpoint(None).await?;
    let wc = WorkingCopy::checkout(client, &dest, &a.rev, sparse, WcOptions::default()).await?;
    let st = wc.state().clone();
    // Persist an explicitly chosen endpoint so later commands in this working copy reach the
    // same server (SPEC 11.1 `.scion/config.toml`).
    let pinned =
        (ctx.global.endpoint.is_some() || ctx.var("SCION_ENDPOINT").is_some()).then_some(endpoint);
    write_wc_config(
        &dest,
        &WcConfig {
            endpoint: pinned,
            tracking_ref: st.tracking_ref.as_ref().map(|r| r.as_str().to_owned()),
        },
    )?;
    let collisions: Vec<CollisionView> = wc
        .collisions()
        .iter()
        .map(|(w, s)| CollisionView {
            written: path_text(w),
            skipped: path_text(s),
        })
        .collect();
    let mut human = format!("Checked out {} into {}\n", base_label(&st), dest.display());
    for c in &collisions {
        let _ = writeln!(
            human,
            "warning: case collision: {} not written (collides with {})",
            c.skipped, c.written
        );
    }
    if !st.inaccessible.is_empty() {
        let _ = writeln!(
            human,
            "note: {} inaccessible subtree(s) not materialized",
            st.inaccessible.len()
        );
    }
    let data = CheckoutData {
        root: dest.display().to_string(),
        base: st.base.to_text(),
        tracking_ref: st.tracking_ref.as_ref().map(|r| r.as_str().to_owned()),
        sparse: paths_text(&st.sparse),
        inaccessible: paths_text(&st.inaccessible),
        collisions,
    };
    Reply::new("checkout", data, human)
}

/// `scion sparse add|remove` (SPEC 11.2).
pub(crate) async fn sparse(ctx: &mut Ctx<'_>, c: &SparseCmd) -> CliResult<Reply> {
    let mut wc = open(ctx).await?;
    let (action, raw) = match c {
        SparseCmd::Add { paths } => ("add", paths),
        SparseCmd::Remove { paths, .. } => ("remove", paths),
    };
    let paths = raw
        .iter()
        .map(|s| parse_path(s))
        .collect::<CliResult<Vec<_>>>()?;
    match c {
        SparseCmd::Add { .. } => wc.sparse_add(&paths).await?,
        SparseCmd::Remove { force, .. } => wc.sparse_remove(&paths, *force).await?,
    }
    let sparse = paths_text(&wc.state().sparse);
    let human = format!(
        "Sparse profile: {}\n",
        sparse
            .iter()
            .map(|p| if p.is_empty() { "/" } else { p.as_str() })
            .collect::<Vec<_>>()
            .join(", ")
    );
    let data = SparseData {
        action: action.to_owned(),
        paths: paths_text(&paths),
        sparse,
    };
    Reply::new("sparse", data, human)
}

/// `scion status` (SPEC 11.5).
pub(crate) async fn status(ctx: &mut Ctx<'_>) -> CliResult<Reply> {
    let mut wc = open_lazy(ctx)?;
    let r = wc.status().await?;
    let st = wc.state().clone();
    let conflicts = unresolved(&st);
    let mut human = format!("On {}\n", base_label(&st));
    for (letter, list) in [
        ('M', &r.modified),
        ('A', &r.added),
        ('D', &r.deleted),
        ('T', &r.mode_changed),
        ('R', &r.redacted_dirty),
    ] {
        for p in list {
            let _ = writeln!(human, "{letter} {}", path_text(p));
        }
    }
    if let Some(p) = &st.pending {
        let _ = writeln!(human, "pending operation: {}", pending_kind(p));
    }
    for c in &conflicts {
        let _ = writeln!(human, "C {c} (unresolved conflict)");
    }
    if r.is_clean() {
        human.push_str("nothing to commit, working copy clean\n");
    }
    let data = StatusData {
        base: st.base.to_text(),
        tracking_ref: st.tracking_ref.as_ref().map(|r| r.as_str().to_owned()),
        modified: paths_text(&r.modified),
        added: paths_text(&r.added),
        deleted: paths_text(&r.deleted),
        mode_changed: paths_text(&r.mode_changed),
        redacted_dirty: paths_text(&r.redacted_dirty),
        hashed: r.hashed as u64,
        pending: st.pending.as_ref().map(|p| pending_kind(p).to_owned()),
        conflicts,
        clean: r.is_clean(),
    };
    Reply::new("status", data, human)
}

fn status_letter(s: &str) -> char {
    match s {
        "modified" => 'M',
        "added" => 'A',
        "deleted" => 'D',
        "mode_changed" => 'T',
        _ => '?',
    }
}

/// `scion diff`: changed files against the base (SPEC 11.5).
pub(crate) async fn diff(ctx: &mut Ctx<'_>) -> CliResult<Reply> {
    let mut wc = open_lazy(ctx)?;
    let changes = wc.diff().await?;
    let mut human = String::new();
    for c in &changes {
        let ids = match (&c.old_id, &c.new_id) {
            (Some(o), Some(n)) => {
                format!("  {} -> {}", &o[..o.len().min(14)], &n[..n.len().min(14)])
            }
            (None, Some(n)) => format!("  -> {}", &n[..n.len().min(14)]),
            (Some(o), None) => format!("  {} ->", &o[..o.len().min(14)]),
            (None, None) => String::new(),
        };
        let _ = writeln!(human, "{} {}{ids}", status_letter(&c.status), c.path);
    }
    if changes.is_empty() {
        human.push_str("no changes\n");
    }
    let data = DiffData {
        base: wc.state().base.to_text(),
        changes,
    };
    Reply::new("diff", data, human)
}

fn delta_view(r: &CommitReport) -> Vec<DeltaView> {
    r.delta
        .iter()
        .map(|d| match &d.op {
            DeltaOp::Put { mode, id, size } => DeltaView {
                path: path_text(&d.path),
                op: "put".to_owned(),
                mode: Some(*mode as u8),
                id: Some(id.to_text()),
                size: Some(*size),
            },
            DeltaOp::Delete => DeltaView {
                path: path_text(&d.path),
                op: "delete".to_owned(),
                mode: None,
                id: None,
                size: None,
            },
        })
        .collect()
}

/// The commit author: `SCION_AUTHOR`, else the authenticated principal (SPEC 4.3.3).
async fn author(ctx: &mut Ctx<'_>, root: &Path) -> CliResult<Vec<u8>> {
    if let Some(a) = ctx.var(ENV_AUTHOR) {
        return Ok(a.into_bytes());
    }
    let client = ctx.client(Some(root)).await?;
    let r = client
        .auth()
        .who_am_i(v1::WhoAmIRequest {})
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    Ok(r.principal.into_bytes())
}

fn commit_reply(cmd: &str, wc: &WorkingCopy, r: &CommitReport, verb: &str) -> CliResult<Reply> {
    let st = wc.state();
    let pending = st.pending.as_ref().map(|p| pending_kind(p).to_owned());
    let conflicts = unresolved(st);
    let mut human = format!(
        "{verb} {} ({} file(s), {} blob(s) uploaded)\n",
        short(&r.changeset),
        r.delta.len(),
        r.uploaded
    );
    if st.base != r.changeset {
        let _ = writeln!(human, "now at {}", base_label(st));
    }
    for c in &conflicts {
        let _ = writeln!(human, "C {c} (rebase paused on conflicts)");
    }
    let data = CommitData {
        changeset: r.changeset.to_text(),
        root_tree: r.root_tree.to_text(),
        change_id: r.change_id.to_hex(),
        parents: r.parents.iter().map(|p| p.to_text()).collect(),
        delta: delta_view(r),
        uploaded: r.uploaded as u64,
        base: st.base.to_text(),
        pending,
    };
    let reply = Reply::new(cmd, data, human)?;
    Ok(if conflicts.is_empty() {
        reply
    } else {
        reply.with_exit(EXIT_CONFLICTS)
    })
}

/// `scion commit -m <msg> [--path p...]` (SPEC 11.6; continues a paused rebase, SPEC 11.7).
pub(crate) async fn commit(ctx: &mut Ctx<'_>, a: &CommitArgs) -> CliResult<Reply> {
    let root = ctx.require_root()?;
    let mut wc = open(ctx).await?;
    let rebasing = matches!(wc.state().pending, Some(PendingOperation::Rebase { .. }));
    // A commit that continues a rebase keeps the original author and message (SPEC 11.7).
    let (message, author) = match (&a.message, rebasing) {
        (_, true) => (a.message.clone().unwrap_or_default(), Vec::new()),
        (Some(m), false) => (m.clone(), author(ctx, &root).await?),
        (None, false) => return Err(usage("commit needs -m <message>")),
    };
    let paths = a
        .paths
        .iter()
        .map(|s| parse_path(s))
        .collect::<CliResult<Vec<_>>>()?;
    let r = wc
        .commit_with_report(
            message.as_bytes(),
            &author,
            (!paths.is_empty()).then_some(paths.as_slice()),
        )
        .await?;
    commit_reply("commit", &wc, &r, "Committed")
}

/// `scion amend [-m <msg>]` (SPEC 11.6).
pub(crate) async fn amend(ctx: &mut Ctx<'_>, a: &AmendArgs) -> CliResult<Reply> {
    let mut wc = open(ctx).await?;
    let r = wc
        .amend_with_report(a.message.as_deref().map(str::as_bytes))
        .await?;
    commit_reply("amend", &wc, &r, "Amended as")
}

/// `scion goto <rev> [--force]` (SPEC 11.3).
pub(crate) async fn goto(ctx: &mut Ctx<'_>, a: &GotoArgs) -> CliResult<Reply> {
    let mut wc = open(ctx).await?;
    let r = wc.goto(&a.rev, a.force).await?;
    let short_of = |s: &str| s.chars().take(14).collect::<String>();
    let mut human = format!(
        "Moved from {} to {}: {} written, {} removed\n",
        short_of(&r.from),
        short_of(&r.to),
        r.written,
        r.removed
    );
    for p in &r.preserved_dirty {
        let _ = writeln!(human, "kept dirty file {p}");
    }
    Reply::new("goto", r, human)
}

/// `scion rebase --onto <rev> | --abort` (SPEC 11.7). Conflicts exit with code 3.
pub(crate) async fn rebase(ctx: &mut Ctx<'_>, a: &RebaseArgs) -> CliResult<Reply> {
    let mut wc = open(ctx).await?;
    if a.abort {
        wc.rebase_abort().await?;
        let st = wc.state();
        let data = RebaseData {
            aborted: true,
            base: st.base.to_text(),
            onto: None,
            rebased: Vec::new(),
            conflicts: Vec::new(),
            head: None,
        };
        let human = format!("Rebase aborted; back at {}\n", base_label(st));
        return Reply::new("rebase", data, human);
    }
    let Some(onto) = &a.onto else {
        return Err(usage("rebase needs --onto <rev> or --abort"));
    };
    let r = wc.rebase(onto).await?;
    let st = wc.state();
    let mut human = String::new();
    if r.conflicts.is_empty() {
        let _ = writeln!(
            human,
            "Rebased {} changeset(s) onto {}; now at {}",
            r.rebased.len(),
            r.onto.chars().take(14).collect::<String>(),
            base_label(st)
        );
    } else {
        human.push_str("Rebase paused on conflicts:\n");
        for c in &r.conflicts {
            let _ = writeln!(human, "C {c}");
        }
        human.push_str(
            "Fix them, run `scion resolve <path>` for each, then `scion commit` to continue \
             (or `scion rebase --abort`).\n",
        );
    }
    let exit = if r.conflicts.is_empty() {
        super::error::EXIT_OK
    } else {
        EXIT_CONFLICTS
    };
    let data = RebaseData {
        aborted: false,
        base: st.base.to_text(),
        onto: Some(r.onto),
        rebased: r.rebased,
        conflicts: r.conflicts,
        head: r.head,
    };
    Ok(Reply::new("rebase", data, human)?.with_exit(exit))
}

/// `scion resolve <path> [--force]` (SPEC 11.7).
pub(crate) async fn resolve(ctx: &mut Ctx<'_>, a: &ResolveArgs) -> CliResult<Reply> {
    let mut wc = open(ctx).await?;
    let p = parse_file_path(&a.path)?;
    wc.resolve(&p, a.force)?;
    let remaining = unresolved(wc.state());
    let human = if remaining.is_empty() {
        format!(
            "Resolved {}; all conflicts resolved, run `scion commit` to continue\n",
            path_text(&p)
        )
    } else {
        format!(
            "Resolved {}; {} conflict(s) remain\n",
            path_text(&p),
            remaining.len()
        )
    };
    let data = ResolveData {
        path: path_text(&p),
        remaining,
    };
    Reply::new("resolve", data, human)
}

/// `scion land [--ref <ref>]` (SPEC 11.7).
pub(crate) async fn land(ctx: &mut Ctx<'_>, a: &LandArgs) -> CliResult<Reply> {
    let mut wc = open(ctx).await?;
    let r = match &a.ref_name {
        Some(n) => RefName::parse(n)?,
        None => wc
            .state()
            .tracking_ref
            .clone()
            .unwrap_or_else(RefName::main),
    };
    let rep = wc.land(&r).await?;
    let mut human = format!(
        "Landed on {}@{}: {} ({} changeset(s))\n",
        rep.ref_name,
        rep.version,
        rep.target.chars().take(14).collect::<String>(),
        rep.landed.len()
    );
    if !rep.moved {
        human.push_str("note: dirty files kept the working copy at its previous base\n");
    }
    Reply::new("land", rep, human)
}
