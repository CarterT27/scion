//! History queries: log, blame, show, cat (SPEC 7.4, 9.2, 11.8, 13.4).

use super::args::{BlameArgs, CatArgs, LogArgs, ShowArgs};
use super::ctx::{Ctx, Reply, SCION_DIR};
use super::error::{CliResult, local, usage};
use super::util::{
    REDACTED, cs_from_item, encode_content, fmt_time, lossy, meta_json, parse_file_path,
    parse_path, path_text, resolve, summary,
};
use crate::output::views::{
    BlameData, BlameRangeView, CatData, HunkView, LogData, LogEntryView, MetaJson, ShowChange,
    ShowData,
};
use futures::StreamExt;
use scion_core::{EntryInfo, ObjectId, Result as ScionResult, ScionError};
use scion_object::RepoPath;
use scion_proto::convert::{opt_entry_from_proto, required_id};
use scion_proto::{ScionClient, error_from_status, v1};
use std::fmt::Write as _;
use std::path::Path;

/// Largest `Log` page requested at once (the server clamps anyway).
const LOG_PAGE: u32 = 1000;

/// The default revision: the working-copy base, else `main`.
fn default_rev(root: Option<&Path>) -> String {
    root.and_then(|r| scion_wc::state::load(&r.join(SCION_DIR)).ok())
        .map(|s| s.base.to_text())
        .unwrap_or_else(|| "main".to_owned())
}

/// Client plus the resolved revision (`rev` or the default).
async fn client_at(ctx: &mut Ctx<'_>, rev: Option<&str>) -> CliResult<(ScionClient, ObjectId)> {
    let root = ctx.find_root();
    let client = ctx.client(root.as_deref()).await?;
    let rev = match rev {
        Some(r) => r.to_owned(),
        None => default_rev(root.as_deref()),
    };
    let id = resolve(&client, &rev).await?;
    Ok((client, id))
}

/// One page of `Log` (SPEC 7.4).
pub(crate) async fn log_page(
    client: &ScionClient,
    from: &ObjectId,
    path: Option<&RepoPath>,
    first_parent: bool,
    limit: u32,
    token: Vec<u8>,
) -> ScionResult<v1::LogResponse> {
    client
        .query()
        .log(v1::LogRequest {
            from: from.as_bytes().to_vec(),
            path: path.map(|p| p.as_bytes().to_vec()),
            first_parent_only: first_parent,
            limit,
            page_token: token,
        })
        .await
        .map(tonic::Response::into_inner)
        .map_err(|s| error_from_status(&s))
}

/// The `--json` view of a `LogEntry`.
pub(crate) fn entry_view(e: &v1::LogEntry) -> ScionResult<LogEntryView> {
    let item = e
        .changeset
        .as_ref()
        .ok_or_else(|| scion_proto::convert::missing_field("changeset"))?;
    let cs = cs_from_item(item)?;
    Ok(LogEntryView {
        id: cs.id.to_text(),
        parents: cs.parents.iter().map(|p| p.to_text()).collect(),
        root_tree: cs.root_tree.to_text(),
        change_id: cs.change_id.to_hex(),
        filtered: cs.filtered,
        meta: meta_json(e.meta.as_ref(), &cs.meta),
    })
}

fn short_text(id_text: &str) -> String {
    id_text.chars().take(14).collect()
}

/// Human rendering of a Meta: author, date and indented message (`<redacted>` when redacted,
/// SPEC 13.4).
fn render_meta(out: &mut String, m: &MetaJson) {
    if m.redacted {
        let _ = writeln!(out, "Author: {REDACTED}");
        let _ = writeln!(out);
        let _ = writeln!(out, "    {REDACTED}");
        return;
    }
    let _ = writeln!(out, "Author: {}", m.author.as_deref().unwrap_or(""));
    if let Some(t) = &m.author_time {
        let _ = writeln!(out, "Date:   {}", fmt_time(t));
    }
    if let Some(c) = &m.committer {
        let _ = writeln!(out, "Committer: {c}");
    }
    let _ = writeln!(out);
    for line in m.message.as_deref().unwrap_or("").lines() {
        let _ = writeln!(out, "    {line}");
    }
}

/// `scion log [path] [--rev] [--first-parent] [--limit]` (SPEC 7.4, 13.4).
pub(crate) async fn log(ctx: &mut Ctx<'_>, a: &LogArgs) -> CliResult<Reply> {
    let (client, from) = client_at(ctx, a.rev.as_deref()).await?;
    let path = a.path.as_deref().map(parse_path).transpose()?;
    let mut entries = Vec::new();
    let mut token = Vec::new();
    let mut truncated = false;
    loop {
        let page = match a.limit {
            Some(l) => {
                let left = l.saturating_sub(entries.len() as u32);
                if left == 0 {
                    truncated = !token.is_empty();
                    break;
                }
                left.min(LOG_PAGE)
            }
            None => LOG_PAGE,
        };
        let r = log_page(&client, &from, path.as_ref(), a.first_parent, page, token).await?;
        for e in &r.entries {
            entries.push(entry_view(e)?);
        }
        token = r.next_page_token;
        if token.is_empty() {
            break;
        }
    }
    if let Some(l) = a.limit {
        entries.truncate(l as usize);
    }
    let mut human = String::new();
    for e in &entries {
        if a.oneline {
            let _ = writeln!(human, "{} {}", short_text(&e.id), summary(&e.meta));
            continue;
        }
        let _ = writeln!(human, "changeset {}", short_text(&e.id));
        if e.parents.len() > 1 {
            let ps: Vec<String> = e.parents.iter().map(|p| short_text(p)).collect();
            let _ = writeln!(human, "Merge:  {}", ps.join(" "));
        }
        render_meta(&mut human, &e.meta);
        let _ = writeln!(human);
    }
    let data = LogData {
        from: from.to_text(),
        path: path.as_ref().map(path_text),
        first_parent: a.first_parent,
        entries,
        truncated,
    };
    Reply::new("log", data, human)
}

/// `ReadPath` (SPEC 7.4): the entry and the full content.
pub(crate) async fn read_path(
    client: &ScionClient,
    cs: &ObjectId,
    path: &RepoPath,
) -> ScionResult<(EntryInfo, Vec<u8>)> {
    let mut stream = client
        .query()
        .read_path(v1::ReadPathRequest {
            changeset: cs.as_bytes().to_vec(),
            path: path.as_bytes().to_vec(),
        })
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    let mut entry = None;
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|s| error_from_status(&s))?;
        if entry.is_none() {
            entry = opt_entry_from_proto(chunk.entry.as_ref())?;
        }
        data.extend_from_slice(&chunk.data);
        if chunk.r#final {
            break;
        }
    }
    let entry = entry.ok_or_else(|| scion_proto::convert::missing_field("entry"))?;
    Ok((entry, data))
}

/// Split `<rev>:<path>`; the `git:` revision prefix keeps its colon (SPEC 11.8).
fn split_spec(s: &str) -> CliResult<(&str, &str)> {
    let skip = if s.starts_with("git:") { 4 } else { 0 };
    let i = s[skip..]
        .find(':')
        .map(|i| i + skip)
        .ok_or_else(|| usage(format!("expected <rev>:<path>, got {s:?}")))?;
    Ok((&s[..i], &s[i + 1..]))
}

/// `scion cat <rev>:<path>` (SPEC 7.4 ReadPath; REDACTED for redacted content, SPEC 13.4).
pub(crate) async fn cat(ctx: &mut Ctx<'_>, a: &CatArgs) -> CliResult<Reply> {
    let (rev, p) = split_spec(&a.spec)?;
    if rev.is_empty() {
        return Err(usage("missing revision before ':'"));
    }
    let (client, cs) = client_at(ctx, Some(rev)).await?;
    let path = parse_file_path(p)?;
    let (entry, data) = read_path(&client, &cs, &path).await?;
    let (encoding, content) = encode_content(&data);
    let out = CatData {
        changeset: cs.to_text(),
        path: path_text(&path),
        id: entry.id.to_text(),
        mode: entry.mode as u8,
        size: entry.size,
        encoding,
        content,
    };
    Reply::new("cat", out, data)
}

/// `scion blame <path> [--rev]` (SPEC 9.2).
pub(crate) async fn blame(ctx: &mut Ctx<'_>, a: &BlameArgs) -> CliResult<Reply> {
    let (client, cs) = client_at(ctx, a.rev.as_deref()).await?;
    let path = parse_file_path(&a.path)?;
    let r = client
        .query()
        .blame(v1::BlameRequest {
            changeset: cs.as_bytes().to_vec(),
            path: path.as_bytes().to_vec(),
        })
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    let ranges = r
        .ranges
        .iter()
        .map(|g| {
            Ok(BlameRangeView {
                start_line: g.start_line,
                line_count: g.line_count,
                origin_changeset: required_id(&g.origin_changeset, "origin_changeset")?.to_text(),
                origin_start_line: g.origin_start_line,
            })
        })
        .collect::<ScionResult<Vec<_>>>()?;
    // Human output shows the lines next to their origins; a redacted file shows origins only.
    let content = read_path(&client, &cs, &path).await.ok().map(|(_, d)| d);
    let lines: Vec<&[u8]> = content
        .as_deref()
        .map(|d| d.split_inclusive(|b| *b == b'\n').collect())
        .unwrap_or_default();
    let mut human = String::new();
    for g in &ranges {
        for i in 0..g.line_count {
            let n = (g.start_line + i) as usize;
            let text = lines
                .get(n)
                .map(|l| lossy(l).trim_end_matches('\n').to_owned())
                .unwrap_or_default();
            let _ = writeln!(
                human,
                "{} {:>5}) {text}",
                short_text(&g.origin_changeset),
                n + 1
            );
        }
    }
    let data = BlameData {
        changeset: cs.to_text(),
        path: path_text(&path),
        ranges,
    };
    Reply::new("blame", data, human)
}

fn change_status(old: Option<&EntryInfo>, new: Option<&EntryInfo>) -> &'static str {
    match (old, new) {
        (None, _) => "added",
        (_, None) => "deleted",
        (Some(o), Some(n)) if o.id == n.id && o.mode != n.mode => "mode_changed",
        _ => "modified",
    }
}

/// `Diff(a, b)` with content hunks (SPEC 7.4).
async fn diff_items(
    client: &ScionClient,
    a: Option<&ObjectId>,
    b: &ObjectId,
) -> ScionResult<Vec<ShowChange>> {
    let mut stream = client
        .query()
        .diff(v1::DiffRequest {
            a: a.map(|x| x.as_bytes().to_vec()).unwrap_or_default(),
            b: b.as_bytes().to_vec(),
            paths: Vec::new(),
            include_content_hunks: true,
        })
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let item = item.map_err(|s| error_from_status(&s))?;
        let old = opt_entry_from_proto(item.old_entry.as_ref())?;
        let new = opt_entry_from_proto(item.new_entry.as_ref())?;
        out.push(ShowChange {
            path: lossy(&item.path),
            status: change_status(old.as_ref(), new.as_ref()).to_owned(),
            old_id: old.map(|e| e.id.to_text()),
            new_id: new.map(|e| e.id.to_text()),
            old_mode: old.map(|e| e.mode as u8),
            new_mode: new.map(|e| e.mode as u8),
            binary: item.binary,
            hunks: item
                .hunks
                .iter()
                .map(|h| HunkView {
                    old_start: h.old_start,
                    old_lines: h.old_lines,
                    new_start: h.new_start,
                    new_lines: h.new_lines,
                    lines: h
                        .lines
                        .iter()
                        .map(|l| lossy(l).trim_end_matches(['\n', '\r']).to_owned())
                        .collect(),
                })
                .collect(),
        });
    }
    Ok(out)
}

/// `scion show [rev]` (SPEC 11.8; redacted Meta renders `<redacted>`, SPEC 13.4).
pub(crate) async fn show(ctx: &mut Ctx<'_>, a: &ShowArgs) -> CliResult<Reply> {
    let (client, id) = client_at(ctx, a.rev.as_deref()).await?;
    let page = log_page(&client, &id, None, true, 1, Vec::new()).await?;
    let entry = page
        .entries
        .first()
        .ok_or_else(|| ScionError::not_found(format!("changeset {id} not found")))?;
    let view = entry_view(entry)?;
    if view.id != id.to_text() {
        return Err(local(format!(
            "the server answered Log({id}) with {}",
            view.id
        )));
    }
    let first_parent = view
        .parents
        .first()
        .map(|p| ObjectId::parse_text(p))
        .transpose()?;
    let changes = diff_items(&client, first_parent.as_ref(), &id).await?;
    let mut human = format!("changeset {}\n", id.to_text());
    if !view.parents.is_empty() {
        let ps: Vec<String> = view.parents.iter().map(|p| short_text(p)).collect();
        let _ = writeln!(human, "Parents: {}", ps.join(" "));
    }
    let _ = writeln!(human, "Change-Id: {}", view.change_id);
    render_meta(&mut human, &view.meta);
    let _ = writeln!(human);
    for c in &changes {
        let letter = match c.status.as_str() {
            "added" => 'A',
            "deleted" => 'D',
            "mode_changed" => 'T',
            _ => 'M',
        };
        let _ = writeln!(human, "{letter} {}", c.path);
        if c.binary {
            let _ = writeln!(human, "  (binary)");
        }
        for h in &c.hunks {
            let _ = writeln!(
                human,
                "@@ -{},{} +{},{} @@",
                h.old_start, h.old_lines, h.new_start, h.new_lines
            );
            for l in &h.lines {
                let _ = writeln!(human, "{l}");
            }
        }
    }
    let data = ShowData {
        id: view.id,
        parents: view.parents,
        root_tree: view.root_tree,
        change_id: view.change_id,
        filtered: view.filtered,
        meta: view.meta,
        changes,
    };
    Reply::new("show", data, human)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_splitting() {
        assert_eq!(split_spec("main:a/b").unwrap(), ("main", "a/b"));
        assert_eq!(split_spec("main~2:x").unwrap(), ("main~2", "x"));
        let g = format!("git:{}:p", "a".repeat(40));
        let (r, p) = split_spec(&g).unwrap();
        assert_eq!((r.len(), p), (44, "p"));
        assert!(split_spec("main").is_err());
    }
}
