//! Shared helpers: id display, paths, revisions, changeset and Meta views (SPEC 4, 7.4, 11.8,
//! 13.4).

use super::error::{CliResult, local, usage};
use crate::output::views::{MetaJson, TimeView};
use scion_core::{ChangeId, ErrorCode, Kind, ObjectId, Result as ScionResult, ScionError};
use scion_object::{Canonical, Changeset, RepoPath};
use scion_proto::convert::{parse_ids, required_id};
use scion_proto::{ScionClient, error_from_status, v1};

/// Human rendering of a redacted Meta field (SPEC 13.4).
pub(crate) const REDACTED: &str = "<redacted>";

/// Short display form: the 2-character prefix plus 12 hex digits (`c1` + 12 hex).
pub(crate) fn short(id: &ObjectId) -> String {
    let t = id.to_text();
    t.chars().take(14).collect()
}

/// Lossy UTF-8 of bytes.
pub(crate) fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Lossy UTF-8 of a repository path (`""` for the root).
pub(crate) fn path_text(p: &RepoPath) -> String {
    lossy(p.as_bytes())
}

/// Parse a repository path argument: leading and trailing `/` are dropped; `""`, `.` and `/`
/// mean the root (SPEC 4.4).
pub(crate) fn parse_path(s: &str) -> CliResult<RepoPath> {
    let t = s.trim_start_matches('/').trim_end_matches('/');
    if t.is_empty() || t == "." {
        return Ok(RepoPath::root());
    }
    Ok(RepoPath::parse(t.as_bytes())?)
}

/// Parse a non-root file path argument.
pub(crate) fn parse_file_path(s: &str) -> CliResult<RepoPath> {
    let p = parse_path(s)?;
    if p.is_root() {
        return Err(usage(format!("{s:?} is not a file path")));
    }
    Ok(p)
}

/// Resolve a revision to a changeset id (SPEC 11.8): a full changeset id locally, anything
/// else through `QueryService.Resolve`.
pub(crate) async fn resolve(client: &ScionClient, rev: &str) -> ScionResult<ObjectId> {
    if let Ok(id) = ObjectId::parse_text(rev)
        && id.kind() == Kind::Changeset
    {
        return Ok(id);
    }
    let r = client
        .query()
        .resolve(v1::ResolveRequest {
            revision: rev.to_owned(),
        })
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    required_id(&r.changeset, "changeset")
}

/// The fields of a changeset as served (full encoding or filtered view; SPEC 12.6).
#[derive(Clone, Debug)]
pub(crate) struct CsView {
    pub id: ObjectId,
    pub parents: Vec<ObjectId>,
    pub root_tree: ObjectId,
    pub meta: ObjectId,
    pub change_id: ChangeId,
    pub filtered: bool,
}

/// Decode a `ChangesetItem` (SPEC 7.4).
pub(crate) fn cs_from_item(item: &v1::ChangesetItem) -> ScionResult<CsView> {
    let id = required_id(&item.id, "id")?;
    match v1::ItemStatus::try_from(item.status) {
        Ok(v1::ItemStatus::Ok) => {}
        Ok(v1::ItemStatus::Denied) => {
            return Err(ScionError::permission_denied(format!(
                "changeset {id} is not readable"
            )));
        }
        Ok(v1::ItemStatus::Redacted) => return Err(ScionError::redacted(&id)),
        _ => return Err(ScionError::not_found(format!("changeset {id} not found"))),
    }
    match &item.view {
        Some(v1::changeset_item::View::Encoded(b)) => {
            let cs = Changeset::decode(b)?;
            Ok(CsView {
                id,
                parents: cs.parents,
                root_tree: cs.root_tree,
                meta: cs.meta,
                change_id: cs.change_id,
                filtered: false,
            })
        }
        Some(v1::changeset_item::View::FilteredView(f)) => Ok(CsView {
            id,
            parents: parse_ids(&f.parents, "parents")?,
            root_tree: required_id(&f.root_tree, "root_tree")?,
            meta: required_id(&f.meta, "meta")?,
            change_id: scion_proto::convert::required_change_id(&f.change_id, "change_id")?,
            filtered: f.filtered,
        }),
        None => Err(scion_proto::convert::missing_field("view")),
    }
}

/// `GetChangesets` for one id.
pub(crate) async fn fetch_cs(client: &ScionClient, id: &ObjectId) -> ScionResult<CsView> {
    let r = client
        .query()
        .get_changesets(v1::GetChangesetsRequest {
            ids: vec![id.as_bytes().to_vec()],
        })
        .await
        .map_err(|s| error_from_status(&s))?
        .into_inner();
    let item = r
        .items
        .first()
        .ok_or_else(|| ScionError::not_found(format!("changeset {id} not found")))?;
    cs_from_item(item)
}

fn time_view(t: Option<&v1::Timestamp>) -> Option<TimeView> {
    t.map(|t| TimeView {
        seconds: t.seconds,
        tz_offset_minutes: t.tz_offset_minutes,
    })
}

/// The `--json` form of a served Meta (SPEC 13.4: a redacted Meta keeps only its id).
pub(crate) fn meta_json(m: Option<&v1::MetaView>, fallback_id: &ObjectId) -> MetaJson {
    let Some(m) = m else {
        return MetaJson {
            redacted: false,
            meta_id: fallback_id.to_text(),
            author: None,
            author_time: None,
            committer: None,
            committer_time: None,
            creator: None,
            message: None,
        };
    };
    let meta_id = ObjectId::from_bytes(&m.meta_id)
        .map(|i| i.to_text())
        .unwrap_or_else(|_| fallback_id.to_text());
    if m.redacted {
        return MetaJson {
            redacted: true,
            meta_id,
            author: None,
            author_time: None,
            committer: None,
            committer_time: None,
            creator: None,
            message: None,
        };
    }
    MetaJson {
        redacted: false,
        meta_id,
        author: Some(lossy(&m.author)),
        author_time: time_view(m.author_time.as_ref()),
        committer: m.committer.as_deref().map(lossy),
        committer_time: time_view(m.committer_time.as_ref()),
        creator: Some(lossy(&m.creator)),
        message: Some(lossy(&m.message)),
    }
}

/// `2023-11-14T22:13:20Z +0000` style rendering of a timestamp.
pub(crate) fn fmt_time(t: &TimeView) -> String {
    let utc = if t.seconds >= 0 {
        let st = std::time::UNIX_EPOCH + std::time::Duration::from_secs(t.seconds.unsigned_abs());
        humantime::format_rfc3339_seconds(st).to_string()
    } else {
        format!("@{}", t.seconds)
    };
    let sign = if t.tz_offset_minutes < 0 { '-' } else { '+' };
    let off = t.tz_offset_minutes.unsigned_abs();
    format!("{utc} {sign}{:02}{:02}", off / 60, off % 60)
}

/// First line of a Meta for one-line rendering (`<redacted>` when redacted).
pub(crate) fn summary(m: &MetaJson) -> String {
    if m.redacted {
        return REDACTED.to_owned();
    }
    m.message
        .as_deref()
        .and_then(|s| s.lines().next())
        .unwrap_or("")
        .to_owned()
}

/// Parse an object id argument.
pub(crate) fn parse_object_id(s: &str) -> CliResult<ObjectId> {
    ObjectId::parse_text(s).map_err(|e| local(format!("invalid object id {s:?}: {}", e.message)))
}

/// `true` when the error is NOT_FOUND.
pub(crate) fn is_not_found(e: &ScionError) -> bool {
    e.code == ErrorCode::NotFound
}

/// Content as `(encoding, text)`: `utf8` when valid, else `hex`.
pub(crate) fn encode_content(b: &[u8]) -> (String, String) {
    match std::str::from_utf8(b) {
        Ok(s) => ("utf8".to_owned(), s.to_owned()),
        Err(_) => {
            let mut h = String::with_capacity(b.len() * 2);
            for x in b {
                h.push_str(&format!("{x:02x}"));
            }
            ("hex".to_owned(), h)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ids() {
        let id = ObjectId::new(Kind::Changeset, [0xab; 32]);
        assert_eq!(short(&id), "c1abababababab");
        assert_eq!(short(&id).len(), 14);
    }

    #[test]
    fn paths() {
        assert!(parse_path("").unwrap().is_root());
        assert!(parse_path("/").unwrap().is_root());
        assert!(parse_path(".").unwrap().is_root());
        assert_eq!(parse_path("/a/b/").unwrap().as_bytes(), b"a/b");
        assert!(parse_path("a/../b").is_err());
        assert!(parse_file_path("").is_err());
    }

    #[test]
    fn times_and_content() {
        let t = TimeView {
            seconds: 1_700_000_000,
            tz_offset_minutes: -90,
        };
        assert_eq!(fmt_time(&t), "2023-11-14T22:13:20Z -0130");
        assert_eq!(encode_content(b"hi"), ("utf8".into(), "hi".into()));
        assert_eq!(encode_content(&[0xff, 0x00]), ("hex".into(), "ff00".into()));
    }

    #[test]
    fn redacted_meta_renders_placeholder() {
        let id = ObjectId::new(Kind::Meta, [1; 32]);
        let m = meta_json(
            Some(&v1::MetaView {
                redacted: true,
                meta_id: id.as_bytes().to_vec(),
                ..Default::default()
            }),
            &id,
        );
        assert!(m.redacted);
        assert_eq!(summary(&m), REDACTED);
        assert_eq!(m.author, None);
    }
}
