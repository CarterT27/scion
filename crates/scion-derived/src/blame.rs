//! SPEC 9.2 `blame` (lazy, keyed by file history node id; not content-bearing; P24).
//!
//! - **Key.** The 34 raw bytes of a file history node id (kind 0x10, SPEC 9.2 `history`).
//! - **Dependencies.** The blame keys of the node's parent nodes that are file nodes (a parent
//!   node of another kind has no lines, so it can attribute nothing; SPEC 9.2).
//! - **Derivation.** For node `N` with content `c`: diff `c` against the content of the first
//!   parent node with [`scion_merge::line_origins`] and attribute every matching line from that
//!   parent's blame; repeat with subsequent parents for the lines still unattributed; the
//!   remaining lines belong to `N.changeset` (their own line numbers). Redacted or purged
//!   content reads as empty through [`DeriveCtx::blob_or_empty`] (SPEC 13.5), so derivation stays
//!   deterministic after redaction.
//! - **Value.** [`encode_ranges`]: a compact canonical list of runs, where a run joins adjacent
//!   lines with the same origin changeset and consecutive origin lines.
//!
//! The engine derives lazily and iteratively (SPEC 9.1): the dependency walk never recurses, so
//! a long single-file history needs no stack depth.

use crate::engine::DerivedEngine;
use crate::history::{self, NODE_KIND};
use crate::types::{DeriveCtx, DeriveMode, DerivedType, LazyType};
use async_trait::async_trait;
use bytes::Bytes;
use scion_core::{ErrorCode, Kind, ObjectId, Result, ScionError, subcode};
use scion_object::enc::{Decoder, Encoder};
use scion_object::{HistoryNode, NodeKind, RepoPath};
use std::collections::HashMap;

/// Type name.
pub const NAME: &str = "blame";

/// Format version of an encoded blame value ([`encode_ranges`]; SPEC 9.2).
pub const VALUE_FORMAT_VERSION: u64 = 1;

/// The lazy blame type (SPEC 9.2); the version is configurable for version-bump tests
/// (SPEC 19.6).
#[derive(Clone, Debug)]
pub struct BlameType {
    version: u32,
}

impl BlameType {
    /// Blame at version 1 (SPEC 9.2).
    pub fn new() -> Self {
        BlameType { version: 1 }
    }

    /// Blame at version `v` (SPEC 9.1 version bump; 19.6).
    pub fn with_version(v: u32) -> Self {
        BlameType { version: v }
    }
}

impl Default for BlameType {
    fn default() -> Self {
        BlameType::new()
    }
}

impl DerivedType for BlameType {
    fn name(&self) -> &'static str {
        NAME
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn mode(&self) -> DeriveMode {
        DeriveMode::Lazy
    }

    fn content_bearing(&self) -> bool {
        false
    }
}

/// Parse a blame key: the raw id of a history node.
fn key_node_id(key: &[u8]) -> Result<ObjectId> {
    let id = ObjectId::from_bytes(key)?;
    if id.kind() != NODE_KIND {
        return Err(ScionError::invalid(
            subcode::INVALID_KIND,
            format!("blame key {id} is not a history node id"),
        ));
    }
    Ok(id)
}

/// The file parent nodes of `node`, in parent order, with their decoded nodes.
async fn file_parents(
    cx: &dyn DeriveCtx,
    node: &HistoryNode,
) -> Result<Vec<(ObjectId, HistoryNode)>> {
    let mut out = Vec::with_capacity(node.parents.len());
    for p in &node.parents {
        let pn = history::get_node(cx, p).await?;
        if pn.kind == NodeKind::File {
            out.push((*p, pn));
        }
    }
    Ok(out)
}

#[async_trait]
impl LazyType for BlameType {
    /// SPEC 9.2: the blame keys of the node's file parent nodes, in parent order. A directory
    /// node has no dependencies (and an empty blame).
    async fn deps(&self, cx: &dyn DeriveCtx, key: &[u8]) -> Result<Vec<Bytes>> {
        let id = key_node_id(key)?;
        let node = history::get_node(cx, &id).await?;
        if node.kind != NodeKind::File {
            return Ok(Vec::new());
        }
        Ok(file_parents(cx, &node)
            .await?
            .into_iter()
            .map(|(p, _)| Bytes::copy_from_slice(p.as_bytes()))
            .collect())
    }

    /// SPEC 9.2 blame derivation (see the module documentation). Deterministic: the value
    /// depends only on the node, the parents' contents and the parents' blame values.
    async fn derive(
        &self,
        cx: &dyn DeriveCtx,
        key: &[u8],
        deps: &[(Bytes, Bytes)],
    ) -> Result<Bytes> {
        let id = key_node_id(key)?;
        let node = history::get_node(cx, &id).await?;
        if node.kind != NodeKind::File {
            return Ok(encode_ranges(&[]));
        }
        let content = cx.blob_or_empty(&node.content).await?;
        let line_count = scion_merge::split_lines(&content).len();
        let mut origins: Vec<Option<(ObjectId, u32)>> = vec![None; line_count];
        let dep_values: HashMap<&[u8], &Bytes> =
            deps.iter().map(|(k, v)| (k.as_ref(), v)).collect();
        for (pid, pnode) in file_parents(cx, &node).await? {
            if origins.iter().all(Option::is_some) {
                break;
            }
            let Some(value) = dep_values.get(&pid.as_bytes()[..]) else {
                return Err(ScionError::new(
                    ErrorCode::InvalidArgument,
                    format!("blame of {id}: no value for parent node {pid}"),
                ));
            };
            let parent_lines = expand_lines(&decode_ranges(value)?);
            let parent_content = cx.blob_or_empty(&pnode.content).await?;
            let matches = scion_merge::line_origins(&parent_content, &content);
            for (slot, m) in origins.iter_mut().zip(matches) {
                if slot.is_none()
                    && let Some(i) = m
                {
                    *slot = parent_lines.get(i).copied();
                }
            }
        }
        let lines: Vec<(ObjectId, u32)> = origins
            .into_iter()
            .enumerate()
            .map(|(j, o)| o.unwrap_or((node.changeset, line_u32(j))))
            .collect();
        Ok(encode_ranges(&ranges_from_lines(&lines)))
    }
}

/// A line index as `u32` (blame line numbers are `u32` on the wire, SPEC 7.4).
fn line_u32(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

/// Per-line `(origin_changeset, origin_line)` of `ranges`, indexed by line. Gaps (never
/// produced by blame) are left out; callers only expand values produced by
/// [`ranges_from_lines`].
fn expand_lines(ranges: &[BlameRange]) -> Vec<(ObjectId, u32)> {
    let total: usize = ranges.iter().map(|r| r.line_count as usize).sum();
    let mut out = Vec::with_capacity(total);
    for r in ranges {
        for k in 0..r.line_count {
            out.push((r.origin_changeset, r.origin_start_line.saturating_add(k)));
        }
    }
    out
}

/// Runs of `lines` (line `i` has origin `lines[i]`): adjacent lines with the same origin
/// changeset and consecutive origin lines are joined (SPEC 9.2; the same rule as the
/// reference model).
pub fn ranges_from_lines(lines: &[(ObjectId, u32)]) -> Vec<BlameRange> {
    let mut out: Vec<BlameRange> = Vec::new();
    for (i, (origin, oline)) in lines.iter().enumerate() {
        let i = line_u32(i);
        if let Some(last) = out.last_mut()
            && mergeable(last, i, origin, *oline)
        {
            last.line_count += 1;
            continue;
        }
        out.push(BlameRange {
            start_line: i,
            line_count: 1,
            origin_changeset: *origin,
            origin_start_line: *oline,
        });
    }
    out
}

/// Can a run starting at line `start` with origin `(origin, ostart)` extend `last`?
fn mergeable(last: &BlameRange, start: u32, origin: &ObjectId, ostart: u32) -> bool {
    last.origin_changeset == *origin
        && u64::from(last.start_line) + u64::from(last.line_count) == u64::from(start)
        && u64::from(last.origin_start_line) + u64::from(last.line_count) == u64::from(ostart)
}

/// A run of lines attributed to one origin (SPEC 7.4 Blame).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameRange {
    /// First line (0-based) in the blamed file.
    pub start_line: u32,
    /// Number of lines.
    pub line_count: u32,
    /// Changeset that introduced the lines.
    pub origin_changeset: ObjectId,
    /// First line in the origin's version of the file.
    pub origin_start_line: u32,
}

/// Normalize ranges for encoding: drop empty runs, order by `start_line`, trim overlaps (a
/// later run loses the lines an earlier one covers), and join mergeable neighbours.
fn normalize(r: &[BlameRange]) -> Vec<BlameRange> {
    let mut sorted: Vec<BlameRange> = r.iter().filter(|x| x.line_count > 0).cloned().collect();
    sorted.sort_by_key(|x| x.start_line);
    let mut out: Vec<BlameRange> = Vec::with_capacity(sorted.len());
    for mut x in sorted {
        if let Some(last) = out.last_mut() {
            let end = u64::from(last.start_line) + u64::from(last.line_count);
            if u64::from(x.start_line) < end {
                let cut = end - u64::from(x.start_line);
                if cut >= u64::from(x.line_count) {
                    continue;
                }
                let cut = u32::try_from(cut).unwrap_or(u32::MAX);
                x.start_line = x.start_line.saturating_add(cut);
                x.line_count -= cut;
                x.origin_start_line = x.origin_start_line.saturating_add(cut);
            }
            if mergeable(last, x.start_line, &x.origin_changeset, x.origin_start_line) {
                last.line_count = last.line_count.saturating_add(x.line_count);
                continue;
            }
        }
        out.push(x);
    }
    out
}

/// Compact canonical encoding of ranges (SPEC 9.2 blame value; adjacent equal-origin lines
/// merged).
///
/// Layout (SPEC 4.1 primitives): `uvarint format_version (1), uvarint count`, then per run
/// `uvarint gap` (lines since the end of the previous run; always 0 in blame values),
/// `uvarint line_count (> 0)`, `id origin_changeset`, `uvarint origin_start_line`. Runs are
/// ordered, non-overlapping and maximally joined; the input is normalized to that form first
/// (empty runs dropped, overlaps trimmed, mergeable neighbours joined).
pub fn encode_ranges(r: &[BlameRange]) -> Bytes {
    let runs = normalize(r);
    let mut e = Encoder::new();
    e.uvarint(VALUE_FORMAT_VERSION);
    e.uvarint(runs.len() as u64);
    let mut end: u32 = 0;
    for x in &runs {
        e.uvarint(u64::from(x.start_line.saturating_sub(end)));
        e.uvarint(u64::from(x.line_count));
        e.id(&x.origin_changeset);
        e.uvarint(u64::from(x.origin_start_line));
        end = x.start_line.saturating_add(x.line_count);
    }
    Bytes::from(e.finish())
}

fn malformed(m: impl Into<String>) -> ScionError {
    ScionError::new(
        ErrorCode::InvalidArgument,
        format!("malformed blame value: {}", m.into()),
    )
}

fn decode_u32(d: &mut Decoder<'_>, what: &str) -> Result<u32> {
    u32::try_from(d.uvarint()?).map_err(|_| malformed(format!("{what} exceeds u32")))
}

/// Inverse of [`encode_ranges`] (SPEC 9.2). Rejects every non-canonical value: an unknown
/// format version, empty runs, non-changeset origins, runs that should have been joined, line
/// numbers beyond `u32`, trailing bytes.
pub fn decode_ranges(b: &[u8]) -> Result<Vec<BlameRange>> {
    let mut d = Decoder::new(b);
    d.format_version(VALUE_FORMAT_VERSION)?;
    let n = d.list_len()?;
    let mut out: Vec<BlameRange> = Vec::with_capacity(n.min(4096));
    let mut end: u64 = 0;
    let limit = u64::from(u32::MAX) + 1;
    for _ in 0..n {
        let gap = decode_u32(&mut d, "gap")?;
        let line_count = decode_u32(&mut d, "line_count")?;
        if line_count == 0 {
            return Err(malformed("empty run"));
        }
        let origin_changeset = d.id()?;
        if origin_changeset.kind() != Kind::Changeset {
            return Err(malformed(format!(
                "origin {origin_changeset} is not a changeset id"
            )));
        }
        let origin_start_line = decode_u32(&mut d, "origin_start_line")?;
        let start = end + u64::from(gap);
        if start + u64::from(line_count) > limit
            || u64::from(origin_start_line) + u64::from(line_count) > limit
        {
            return Err(malformed("run exceeds u32 line numbers"));
        }
        let start_line = u32::try_from(start).map_err(|_| malformed("start_line exceeds u32"))?;
        if let Some(last) = out.last()
            && mergeable(last, start_line, &origin_changeset, origin_start_line)
        {
            return Err(malformed("adjacent runs are not joined"));
        }
        out.push(BlameRange {
            start_line,
            line_count,
            origin_changeset,
            origin_start_line,
        });
        end = start + u64::from(line_count);
    }
    d.finish()?;
    Ok(out)
}

/// Blame `path` at changeset `cs` (SPEC 7.4 Blame, 9.2): resolves the path's history node
/// (`history::node_for_path`) and returns the lazily derived `blame` value of that node at
/// `version` (`None` = the latest registered version; an explicit older version reads its
/// stored values, SPEC 9.1).
///
/// Errors: NOT_FOUND for a missing changeset, or a missing or non-file path.
pub async fn blame(
    e: &DerivedEngine,
    cs: &ObjectId,
    path: &RepoPath,
    version: Option<u32>,
) -> Result<Vec<BlameRange>> {
    let Some((node_id, node)) = history::node_for_path(e.ctx(), cs, path).await? else {
        return Err(ScionError::not_found(format!(
            "path not found at changeset {cs}"
        )));
    };
    if node.kind != NodeKind::File {
        return Err(ScionError::not_found(format!(
            "path is not a file at changeset {cs}"
        )));
    }
    let value = e.get(NAME, version, node_id.as_bytes()).await?;
    decode_ranges(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(n: u8) -> ObjectId {
        ObjectId::new(Kind::Changeset, [n; 32])
    }

    fn r(start: u32, count: u32, origin: u8, ostart: u32) -> BlameRange {
        BlameRange {
            start_line: start,
            line_count: count,
            origin_changeset: cs(origin),
            origin_start_line: ostart,
        }
    }

    #[test]
    fn round_trip_and_canonical_joining() {
        let v = vec![r(0, 2, 1, 0), r(2, 1, 2, 5), r(3, 4, 1, 2)];
        let enc = encode_ranges(&v);
        assert_eq!(decode_ranges(&enc).unwrap(), v);
        // Mergeable neighbours are joined by the encoder.
        let split = vec![r(0, 1, 1, 0), r(1, 1, 1, 1), r(2, 1, 2, 0)];
        let joined = decode_ranges(&encode_ranges(&split)).unwrap();
        assert_eq!(joined, vec![r(0, 2, 1, 0), r(2, 1, 2, 0)]);
        assert_eq!(encode_ranges(&split), encode_ranges(&joined));
        // Same origin but non-consecutive origin lines stay separate.
        let jump = vec![r(0, 1, 1, 0), r(1, 1, 1, 7)];
        assert_eq!(decode_ranges(&encode_ranges(&jump)).unwrap(), jump);
        // Empty.
        assert!(decode_ranges(&encode_ranges(&[])).unwrap().is_empty());
        // Unsorted, empty and overlapping input is normalized.
        let messy = vec![r(3, 2, 2, 0), r(0, 0, 9, 0), r(0, 4, 1, 0)];
        assert_eq!(
            decode_ranges(&encode_ranges(&messy)).unwrap(),
            vec![r(0, 4, 1, 0), r(4, 1, 2, 1)]
        );
        // Gaps survive.
        let gap = vec![r(2, 1, 1, 0)];
        assert_eq!(decode_ranges(&encode_ranges(&gap)).unwrap(), gap);
    }

    #[test]
    fn decode_rejects_non_canonical_values() {
        let mut e = Encoder::new();
        e.uvarint(1);
        e.uvarint(2);
        for (gap, count, ostart) in [(0u64, 1u64, 0u64), (0, 1, 1)] {
            e.uvarint(gap);
            e.uvarint(count);
            e.id(&cs(1));
            e.uvarint(ostart);
        }
        assert!(decode_ranges(&e.finish()).is_err(), "unjoined runs");

        let mut e = Encoder::new();
        e.uvarint(1);
        e.uvarint(1);
        e.uvarint(0);
        e.uvarint(0);
        e.id(&cs(1));
        e.uvarint(0);
        assert!(decode_ranges(&e.finish()).is_err(), "empty run");

        let mut e = Encoder::new();
        e.uvarint(2);
        e.uvarint(0);
        assert!(decode_ranges(&e.finish()).is_err(), "unknown version");

        let mut e = Encoder::new();
        e.uvarint(1);
        e.uvarint(1);
        e.uvarint(0);
        e.uvarint(1);
        e.id(&ObjectId::hash_blob(b"x"));
        e.uvarint(0);
        assert!(decode_ranges(&e.finish()).is_err(), "blob origin");

        let mut v = encode_ranges(&[r(0, 1, 1, 0)]).to_vec();
        v.push(0);
        assert!(decode_ranges(&v).is_err(), "trailing bytes");
    }

    #[test]
    fn ranges_from_lines_joins_runs() {
        let lines = vec![(cs(1), 0), (cs(1), 1), (cs(2), 0), (cs(1), 3), (cs(1), 4)];
        assert_eq!(
            ranges_from_lines(&lines),
            vec![r(0, 2, 1, 0), r(2, 1, 2, 0), r(3, 2, 1, 3)]
        );
        assert_eq!(expand_lines(&ranges_from_lines(&lines)), lines);
    }

    #[test]
    fn type_descriptor() {
        let t = BlameType::default();
        assert_eq!(t.name(), NAME);
        assert_eq!(t.version(), 1);
        assert_eq!(BlameType::with_version(2).version(), 2);
        assert_eq!(t.mode(), DeriveMode::Lazy);
        assert!(!t.content_bearing());
    }
}
