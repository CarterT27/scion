//! `scion redact` tooling (SPEC 13.2): resolve ids from `--path/--at [--all-versions]`,
//! `--object` or `--meta`, then `AdminService.Redact` (and `PurgeNow` with `--purge-now`).

use super::args::RedactArgs;
use super::ctx::{Ctx, Reply};
use super::error::{CliResult, local, usage};
use super::query::log_page;
use super::util::{
    cs_from_item, fetch_cs, is_not_found, parse_file_path, parse_object_id, resolve, short,
};
use crate::output::views::{RedactData, RedactTarget};
use scion_core::{ErrorCode, Kind, Mode, ObjectId, Result as ScionResult, subcode};
use scion_object::RepoPath;
use scion_proto::convert::{ids_bytes, parse_mode, required_id};
use scion_proto::{ScionClient, error_from_status, v1};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

/// Default audit reason when `--reason` is omitted (the server requires a non-empty reason).
pub(crate) const DEFAULT_REASON: &str = "redacted with scion redact";

/// Page size of the history walk.
const WALK_PAGE: u32 = 1000;

/// The `(mode, id)` at `path` in changeset `cs`, via `ListTree` of its parent directory;
/// `None` when the path (or a directory on the way) is absent (SPEC 7.4).
async fn entry_at(
    client: &ScionClient,
    cs: &ObjectId,
    path: &RepoPath,
) -> ScionResult<Option<(Mode, ObjectId)>> {
    let parent = path.parent().unwrap_or_else(RepoPath::root);
    let Some(name) = path.file_name() else {
        return Ok(None);
    };
    let r = client
        .query()
        .list_tree(v1::ListTreeRequest {
            changeset: cs.as_bytes().to_vec(),
            path: parent.as_bytes().to_vec(),
        })
        .await
        .map_err(|s| error_from_status(&s));
    let r = match r {
        Ok(r) => r.into_inner(),
        Err(e) if is_not_found(&e) => return Ok(None),
        Err(e)
            if e.code == ErrorCode::InvalidArgument
                && e.subcode() == Some(subcode::INVALID_PATH) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    for t in &r.entries {
        if t.name == name {
            let Some(e) = &t.entry else {
                return Ok(None);
            };
            return Ok(Some((parse_mode(e.mode)?, required_id(&e.id, "id")?)));
        }
    }
    Ok(None)
}

/// Every distinct blob `path` ever held in history reachable from `at`, in first-seen order
/// (SPEC 13.2 `--all-versions`, 19.13).
///
/// The walk covers every changeset reachable from `at` (`Log` without a path follows all
/// parents), not only the path's TREESAME history, so versions that lived only on a merged-away
/// branch or before a deletion and re-creation are found. The path lookup (`ListTree` of the
/// parent directory) is memoized by root tree.
pub(crate) async fn all_versions(
    client: &ScionClient,
    at: &ObjectId,
    path: &RepoPath,
) -> ScionResult<Vec<ObjectId>> {
    let mut out = Vec::new();
    let mut found: HashSet<ObjectId> = HashSet::new();
    let mut memo: HashMap<ObjectId, Option<(Mode, ObjectId)>> = HashMap::new();
    let mut token = Vec::new();
    loop {
        let page = log_page(client, at, None, false, WALK_PAGE, token).await?;
        for e in &page.entries {
            let item = e
                .changeset
                .as_ref()
                .ok_or_else(|| scion_proto::convert::missing_field("changeset"))?;
            let cs = cs_from_item(item)?;
            let hit = match memo.get(&cs.root_tree) {
                Some(h) => *h,
                None => {
                    let h = entry_at(client, &cs.id, path).await?;
                    memo.insert(cs.root_tree, h);
                    h
                }
            };
            if let Some((mode, id)) = hit
                && mode.is_file_like()
                && found.insert(id)
            {
                out.push(id);
            }
        }
        token = page.next_page_token;
        if token.is_empty() {
            break;
        }
    }
    tracing::debug!(
        versions = out.len(),
        trees = memo.len(),
        "all-versions walk done"
    );
    Ok(out)
}

fn kind_label(k: Kind) -> &'static str {
    match k {
        Kind::Blob => "blob",
        Kind::Meta => "meta",
        Kind::Tree => "tree",
        Kind::Changeset => "changeset",
        Kind::Derived(_) => "derived",
    }
}

/// `scion redact` (SPEC 13.2; permission `admin.redact`, and `admin.purge` for
/// `--purge-now`).
pub(crate) async fn redact(ctx: &mut Ctx<'_>, a: &RedactArgs) -> CliResult<Reply> {
    let root = ctx.find_root();
    let client = ctx.client(root.as_deref()).await?;
    let ids: Vec<ObjectId> = if let Some(p) = &a.path {
        let at =
            a.at.as_deref()
                .ok_or_else(|| usage("--path needs --at <rev>"))?;
        let at = resolve(&client, at).await?;
        let path = parse_file_path(p)?;
        if a.all_versions {
            let v = all_versions(&client, &at, &path).await?;
            if v.is_empty() {
                return Err(local(format!(
                    "{p} never held a file in history reachable from {}",
                    short(&at)
                )));
            }
            v
        } else {
            match entry_at(&client, &at, &path).await? {
                Some((m, id)) if m.is_file_like() => vec![id],
                Some(_) => return Err(local(format!("{p} is a directory at {}", short(&at)))),
                None => return Err(local(format!("{p} does not exist at {}", short(&at)))),
            }
        }
    } else if let Some(o) = &a.object {
        vec![parse_object_id(o)?]
    } else if let Some(rev) = &a.meta {
        let cs = resolve(&client, rev).await?;
        vec![fetch_cs(&client, &cs).await?.meta]
    } else {
        return Err(usage("one of --path, --object or --meta is required"));
    };
    let reason = a
        .reason
        .clone()
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| DEFAULT_REASON.to_owned());
    let resp = client
        .admin()
        .redact(v1::RedactRequest {
            object_ids: ids_bytes(&ids),
            reason: reason.clone(),
            purge_after_seconds: a.purge_after.map(|d| d.as_secs()),
        })
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    let mut epoch = resp.epoch;
    if a.purge_now {
        client
            .admin()
            .purge_now(v1::PurgeNowRequest {
                object_ids: ids_bytes(&ids),
                reason: reason.clone(),
            })
            .await
            .map_err(|s| error_from_status(&s))?;
        // Purge does not bump the epoch, but report the latest one the server knows.
        if let Some(e) = client.last_epochs() {
            epoch = epoch.max(e.redaction);
        }
    }
    let targets: Vec<RedactTarget> = ids
        .iter()
        .map(|id| RedactTarget {
            id: id.to_text(),
            kind: kind_label(id.kind()).to_owned(),
        })
        .collect();
    let mut human = String::new();
    for t in &targets {
        let _ = writeln!(human, "redacted {} {}", t.kind, t.id);
    }
    let _ = writeln!(
        human,
        "{} object(s) {} (epoch {epoch}); reason: {reason}",
        targets.len(),
        if a.purge_now { "purged" } else { "quarantined" }
    );
    let data = RedactData {
        targets,
        reason,
        epoch,
        effective_by_unix_ms: resp.effective_by_unix_ms,
        purged: a.purge_now,
    };
    Reply::new("redact", data, human)
}
