//! Conversions between `v1` messages and scion-core / scion-object types (PLAN 2.13, P08).
//!
//! `From` / `TryFrom` impls live here. `TryFrom` failures are INVALID_ARGUMENT with subcode
//! `missing_field` (absent required field) or `bad_id_text` (malformed id). Unknown proto
//! fields are ignored (prost default).
//!
//! Scope (amendment O-2): scion-core and scion-object types only. Row types of scion-store
//! (`RefRow`, `RefLogRow`, `LandedEntry`) are converted in scion-server, because scion-proto may
//! not depend on scion-store.
//!
//! Conventions (SPEC 7, PLAN 2.13):
//! - Ids travel as their 34 raw bytes (SPEC 4.2); an empty bytes field means "absent".
//! - Change ids travel as their 16 raw bytes (SPEC 4.3.5).
//! - Paths travel as raw bytes and are validated with [`RepoPath::parse`] (SPEC 4.4).

use crate::v1;
use scion_core::{
    ChangeId, CleanEntry, Conflict, ConflictHunk, ConflictKind, ConflictReport, EntryInfo,
    ErrorCode, ErrorPayload, Limits, Mode, ObjectId, Result, ScionError, subcode,
};
use scion_object::{DeltaEntry, DeltaOp, Extensions, Name, RepoPath, Timestamp, TreeEntry};

/// Delta op value of a Put (`DeltaEntryMsg.op`, PLAN 2.13).
pub const DELTA_OP_PUT: u32 = 1;
/// Delta op value of a Delete (`DeltaEntryMsg.op`, PLAN 2.13).
pub const DELTA_OP_DELETE: u32 = 2;

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// INVALID_ARGUMENT/missing_field naming the absent required field (PLAN 6.7).
pub fn missing_field(field: &str) -> ScionError {
    ScionError::invalid(
        subcode::MISSING_FIELD,
        format!("missing required field `{field}`"),
    )
}

fn bad_id(field: &str, detail: impl std::fmt::Display) -> ScionError {
    ScionError::invalid(
        subcode::BAD_ID_TEXT,
        format!("malformed id in `{field}`: {detail}"),
    )
}

fn invalid_value(field: &str, detail: impl std::fmt::Display) -> ScionError {
    ScionError::new(
        ErrorCode::InvalidArgument,
        format!("invalid value in `{field}`: {detail}"),
    )
}

// ---------------------------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------------------------

/// The wire form of an id: its 34 raw bytes (SPEC 4.2).
pub fn id_bytes(id: &ObjectId) -> Vec<u8> {
    id.as_bytes().to_vec()
}

/// The wire form of an optional id: the 34 raw bytes, or empty for `None` (SPEC 4.2).
pub fn opt_id_bytes(id: Option<&ObjectId>) -> Vec<u8> {
    id.map(id_bytes).unwrap_or_default()
}

/// The wire form of an id list (SPEC 4.2).
pub fn ids_bytes(ids: &[ObjectId]) -> Vec<Vec<u8>> {
    ids.iter().map(id_bytes).collect()
}

/// Parse a present id (SPEC 4.2). Any malformation (length, algorithm, kind) is
/// INVALID_ARGUMENT/bad_id_text; `field` names the proto field in the message.
pub fn parse_id(b: &[u8], field: &str) -> Result<ObjectId> {
    ObjectId::from_bytes(b).map_err(|e| bad_id(field, e.message))
}

/// Parse a required id: empty is INVALID_ARGUMENT/missing_field, otherwise as [`parse_id`].
pub fn required_id(b: &[u8], field: &str) -> Result<ObjectId> {
    if b.is_empty() {
        return Err(missing_field(field));
    }
    parse_id(b, field)
}

/// Parse an optional id: empty is `None`, otherwise as [`parse_id`].
pub fn optional_id(b: &[u8], field: &str) -> Result<Option<ObjectId>> {
    if b.is_empty() {
        Ok(None)
    } else {
        parse_id(b, field).map(Some)
    }
}

/// Parse a repeated id field (each element as [`parse_id`]).
pub fn parse_ids(v: &[Vec<u8>], field: &str) -> Result<Vec<ObjectId>> {
    v.iter().map(|b| parse_id(b, field)).collect()
}

/// The wire form of a change id: its 16 raw bytes (SPEC 4.3.5).
pub fn change_id_bytes(c: &ChangeId) -> Vec<u8> {
    c.0.to_vec()
}

/// Parse a required change id: empty is INVALID_ARGUMENT/missing_field, a length other than 16
/// is INVALID_ARGUMENT/bad_id_text (SPEC 4.3.5).
pub fn required_change_id(b: &[u8], field: &str) -> Result<ChangeId> {
    if b.is_empty() {
        return Err(missing_field(field));
    }
    let arr: [u8; 16] = b.try_into().map_err(|_| {
        bad_id(
            field,
            format!("change id must be 16 bytes, got {}", b.len()),
        )
    })?;
    Ok(ChangeId(arr))
}

/// Parse a mode carried as `uint32` (SPEC 4.3.2): INVALID_ARGUMENT/invalid_mode unless 1-4.
pub fn parse_mode(v: u32) -> Result<Mode> {
    let b = u8::try_from(v)
        .map_err(|_| ScionError::invalid(subcode::INVALID_MODE, format!("invalid mode {v}")))?;
    Mode::from_u8(b)
}

// ---------------------------------------------------------------------------------------------
// Entries, timestamps, extensions, deltas, trees
// ---------------------------------------------------------------------------------------------

impl From<&EntryInfo> for v1::EntryInfo {
    /// SPEC 4.3.2 entry (mode, id, size).
    fn from(e: &EntryInfo) -> Self {
        v1::EntryInfo {
            mode: u32::from(e.mode as u8),
            id: id_bytes(&e.id),
            size: e.size,
        }
    }
}

impl From<EntryInfo> for v1::EntryInfo {
    /// SPEC 4.3.2 entry (mode, id, size).
    fn from(e: EntryInfo) -> Self {
        v1::EntryInfo::from(&e)
    }
}

impl TryFrom<&v1::EntryInfo> for EntryInfo {
    type Error = ScionError;
    /// SPEC 4.3.2 entry; mode must be 1-4 and the id is required.
    fn try_from(m: &v1::EntryInfo) -> Result<Self> {
        Ok(EntryInfo {
            mode: parse_mode(m.mode)?,
            id: required_id(&m.id, "entry.id")?,
            size: m.size,
        })
    }
}

impl TryFrom<v1::EntryInfo> for EntryInfo {
    type Error = ScionError;
    /// SPEC 4.3.2 entry; see the `&v1::EntryInfo` impl.
    fn try_from(m: v1::EntryInfo) -> Result<Self> {
        EntryInfo::try_from(&m)
    }
}

/// Optional entry to the wire (absent = `None`).
pub fn opt_entry_to_proto(e: Option<&EntryInfo>) -> Option<v1::EntryInfo> {
    e.map(v1::EntryInfo::from)
}

/// Optional entry from the wire (absent = `None`).
pub fn opt_entry_from_proto(m: Option<&v1::EntryInfo>) -> Result<Option<EntryInfo>> {
    m.map(EntryInfo::try_from).transpose()
}

impl From<&Timestamp> for v1::Timestamp {
    /// SPEC 4.3.3 timestamp.
    fn from(t: &Timestamp) -> Self {
        v1::Timestamp {
            seconds: t.seconds,
            tz_offset_minutes: t.tz_offset_minutes,
        }
    }
}

impl From<Timestamp> for v1::Timestamp {
    /// SPEC 4.3.3 timestamp.
    fn from(t: Timestamp) -> Self {
        v1::Timestamp::from(&t)
    }
}

impl From<&v1::Timestamp> for Timestamp {
    /// SPEC 4.3.3 timestamp.
    fn from(t: &v1::Timestamp) -> Self {
        Timestamp {
            seconds: t.seconds,
            tz_offset_minutes: t.tz_offset_minutes,
        }
    }
}

impl From<v1::Timestamp> for Timestamp {
    /// SPEC 4.3.3 timestamp.
    fn from(t: v1::Timestamp) -> Self {
        Timestamp::from(&t)
    }
}

/// Extensions to the wire, in ascending tag order (SPEC 4.1).
pub fn extensions_to_proto(x: &Extensions) -> Vec<v1::Extension> {
    x.iter()
        .map(|(tag, value)| v1::Extension {
            tag,
            value: value.to_vec(),
        })
        .collect()
}

/// Extensions from the wire (SPEC 4.1). Tags must be strictly ascending:
/// INVALID_ARGUMENT/unsorted or /duplicate otherwise.
pub fn extensions_from_proto(v: &[v1::Extension]) -> Result<Extensions> {
    for w in v.windows(2) {
        let (p, t) = (w[0].tag, w[1].tag);
        if t == p {
            return Err(ScionError::invalid(
                subcode::DUPLICATE,
                format!("duplicate extension tag {t}"),
            ));
        }
        if t < p {
            return Err(ScionError::invalid(
                subcode::UNSORTED,
                format!("extension tags not ascending at {t}"),
            ));
        }
    }
    let mut out = Extensions::new();
    for e in v {
        out.insert(e.tag, e.value.clone())?;
    }
    Ok(out)
}

impl From<&DeltaEntry> for v1::DeltaEntryMsg {
    /// SPEC 4.3.4 delta entry: op 1 (Put, with mode/id/size) or 2 (Delete).
    fn from(d: &DeltaEntry) -> Self {
        match &d.op {
            DeltaOp::Put { mode, id, size } => v1::DeltaEntryMsg {
                path: d.path.as_bytes().to_vec(),
                op: DELTA_OP_PUT,
                mode: u32::from(*mode as u8),
                id: id_bytes(id),
                size: *size,
            },
            DeltaOp::Delete => v1::DeltaEntryMsg {
                path: d.path.as_bytes().to_vec(),
                op: DELTA_OP_DELETE,
                mode: 0,
                id: Vec::new(),
                size: 0,
            },
        }
    }
}

impl TryFrom<&v1::DeltaEntryMsg> for DeltaEntry {
    type Error = ScionError;
    /// SPEC 4.3.4 delta entry. `op` 0 is INVALID_ARGUMENT/missing_field and any other value but
    /// 1/2 is INVALID_ARGUMENT; a Put requires a mode 1-3 and an id; the path is validated per
    /// SPEC 4.4 (INVALID_ARGUMENT/invalid_path).
    fn try_from(m: &v1::DeltaEntryMsg) -> Result<Self> {
        let op = match m.op {
            DELTA_OP_PUT => {
                if m.mode == 0 {
                    return Err(missing_field("delta.mode"));
                }
                let mode = parse_mode(m.mode)?;
                if !mode.is_file_like() {
                    return Err(ScionError::invalid(
                        subcode::INVALID_MODE,
                        format!("delta put mode must be 1-3, got {}", m.mode),
                    ));
                }
                DeltaOp::Put {
                    mode,
                    id: required_id(&m.id, "delta.id")?,
                    size: m.size,
                }
            }
            DELTA_OP_DELETE => DeltaOp::Delete,
            0 => return Err(missing_field("delta.op")),
            other => {
                return Err(invalid_value(
                    "delta.op",
                    format!("must be 1 (Put) or 2 (Delete), got {other}"),
                ));
            }
        };
        Ok(DeltaEntry {
            path: RepoPath::parse(&m.path)?,
            op,
        })
    }
}

impl TryFrom<v1::DeltaEntryMsg> for DeltaEntry {
    type Error = ScionError;
    /// SPEC 4.3.4 delta entry; see the `&v1::DeltaEntryMsg` impl.
    fn try_from(m: v1::DeltaEntryMsg) -> Result<Self> {
        DeltaEntry::try_from(&m)
    }
}

/// A delta list to the wire (SPEC 4.3.4).
pub fn delta_to_proto(d: &[DeltaEntry]) -> Vec<v1::DeltaEntryMsg> {
    d.iter().map(v1::DeltaEntryMsg::from).collect()
}

/// A delta list from the wire (SPEC 4.3.4). Order and uniqueness are checked later by
/// `Changeset::validate_shape` (SPEC 8.1).
pub fn delta_from_proto(v: &[v1::DeltaEntryMsg]) -> Result<Vec<DeltaEntry>> {
    v.iter().map(DeltaEntry::try_from).collect()
}

impl From<&TreeEntry> for v1::TreeEntryMsg {
    /// SPEC 4.3.2 tree entry.
    fn from(e: &TreeEntry) -> Self {
        v1::TreeEntryMsg {
            name: e.name.as_bytes().to_vec(),
            entry: Some(v1::EntryInfo {
                mode: u32::from(e.mode as u8),
                id: id_bytes(&e.id),
                size: e.size,
            }),
        }
    }
}

impl TryFrom<&v1::TreeEntryMsg> for TreeEntry {
    type Error = ScionError;
    /// SPEC 4.3.2 tree entry; `entry` is required and the name is validated per SPEC 4.4.
    fn try_from(m: &v1::TreeEntryMsg) -> Result<Self> {
        let e = m.entry.as_ref().ok_or_else(|| missing_field("entry"))?;
        let info = EntryInfo::try_from(e)?;
        Ok(TreeEntry {
            name: Name::new(&m.name)?,
            mode: info.mode,
            id: info.id,
            size: info.size,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Meta fields and write requests
// ---------------------------------------------------------------------------------------------

/// The client-supplied Meta fields (`v1::MetaFields`, SPEC 7.5): everything in a Meta object
/// except `creator`, which the server sets. Maps 1:1 onto `scion_history::MetaFields`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetaFieldsParts {
    /// Author identity bytes (SPEC 4.3.3).
    pub author: Vec<u8>,
    /// Author timestamp.
    pub author_time: Timestamp,
    /// Optional committer identity bytes.
    pub committer: Option<Vec<u8>>,
    /// Optional committer timestamp.
    pub committer_time: Option<Timestamp>,
    /// Message bytes.
    pub message: Vec<u8>,
    /// Meta extensions.
    pub extensions: Extensions,
}

impl From<&MetaFieldsParts> for v1::MetaFields {
    /// SPEC 7.5 `meta_fields`.
    fn from(m: &MetaFieldsParts) -> Self {
        v1::MetaFields {
            author: m.author.clone(),
            author_time: Some(v1::Timestamp::from(&m.author_time)),
            committer: m.committer.clone(),
            committer_time: m.committer_time.as_ref().map(v1::Timestamp::from),
            message: m.message.clone(),
            extensions: extensions_to_proto(&m.extensions),
        }
    }
}

impl TryFrom<&v1::MetaFields> for MetaFieldsParts {
    type Error = ScionError;
    /// SPEC 7.5 `meta_fields`; `author_time` is required (INVALID_ARGUMENT/missing_field).
    fn try_from(m: &v1::MetaFields) -> Result<Self> {
        let author_time = m
            .author_time
            .as_ref()
            .ok_or_else(|| missing_field("meta_fields.author_time"))?;
        Ok(MetaFieldsParts {
            author: m.author.clone(),
            author_time: Timestamp::from(author_time),
            committer: m.committer.clone(),
            committer_time: m.committer_time.as_ref().map(Timestamp::from),
            message: m.message.clone(),
            extensions: extensions_from_proto(&m.extensions)?,
        })
    }
}

/// A decoded `CreateChangesetRequest` (SPEC 7.5, 8.1). Maps onto `scion_history::CreateRequest`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateChangesetParts {
    /// Parent changeset ids, in order.
    pub parents: Vec<ObjectId>,
    /// The delta, as sent (order is validated by SPEC 8.1).
    pub delta: Vec<DeltaEntry>,
    /// Meta fields (required).
    pub meta: MetaFieldsParts,
    /// Change id (required, 16 bytes).
    pub change_id: ChangeId,
    /// Optional expected root tree.
    pub expected_root_tree: Option<ObjectId>,
    /// Import mode (PLAN 6.4).
    pub import_mode: bool,
}

impl TryFrom<&v1::CreateChangesetRequest> for CreateChangesetParts {
    type Error = ScionError;
    /// Required fields (`meta_fields`, `change_id`) are checked first, then ids, then the delta.
    fn try_from(m: &v1::CreateChangesetRequest) -> Result<Self> {
        let meta = m
            .meta_fields
            .as_ref()
            .ok_or_else(|| missing_field("meta_fields"))?;
        let change_id = required_change_id(&m.change_id, "change_id")?;
        let meta = MetaFieldsParts::try_from(meta)?;
        let parents = parse_ids(&m.parents, "parents")?;
        let expected_root_tree = match &m.expected_root_tree {
            Some(b) => Some(required_id(b, "expected_root_tree")?),
            None => None,
        };
        let delta = delta_from_proto(&m.delta)?;
        Ok(CreateChangesetParts {
            parents,
            delta,
            meta,
            change_id,
            expected_root_tree,
            import_mode: m.import_mode,
        })
    }
}

impl From<&CreateChangesetParts> for v1::CreateChangesetRequest {
    /// SPEC 7.5 CreateChangeset request.
    fn from(p: &CreateChangesetParts) -> Self {
        v1::CreateChangesetRequest {
            parents: ids_bytes(&p.parents),
            delta: delta_to_proto(&p.delta),
            meta_fields: Some(v1::MetaFields::from(&p.meta)),
            change_id: change_id_bytes(&p.change_id),
            expected_root_tree: p.expected_root_tree.as_ref().map(id_bytes),
            import_mode: p.import_mode,
        }
    }
}

/// A decoded `MergeRequest` (SPEC 7.5): every field is required.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeParts {
    /// First changeset.
    pub a: ObjectId,
    /// Second changeset.
    pub b: ObjectId,
    /// Meta fields of the merge changeset.
    pub meta: MetaFieldsParts,
    /// Change id of the merge changeset.
    pub change_id: ChangeId,
}

impl TryFrom<&v1::MergeRequest> for MergeParts {
    type Error = ScionError;
    /// INVALID_ARGUMENT/missing_field for any absent field; bad_id_text for malformed ids.
    fn try_from(m: &v1::MergeRequest) -> Result<Self> {
        let a = required_id(&m.a, "a")?;
        let b = required_id(&m.b, "b")?;
        let meta = m
            .meta_fields
            .as_ref()
            .ok_or_else(|| missing_field("meta_fields"))?;
        let change_id = required_change_id(&m.change_id, "change_id")?;
        Ok(MergeParts {
            a,
            b,
            meta: MetaFieldsParts::try_from(meta)?,
            change_id,
        })
    }
}

impl From<&MergeParts> for v1::MergeRequest {
    /// SPEC 7.5 Merge request.
    fn from(p: &MergeParts) -> Self {
        v1::MergeRequest {
            a: id_bytes(&p.a),
            b: id_bytes(&p.b),
            meta_fields: Some(v1::MetaFields::from(&p.meta)),
            change_id: change_id_bytes(&p.change_id),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Conflict payloads (SPEC 8.3)
// ---------------------------------------------------------------------------------------------

fn range_u32(start: u32, end: u32, field: &str) -> Result<std::ops::Range<u32>> {
    if end < start {
        return Err(invalid_value(
            field,
            format!("range end {end} < start {start}"),
        ));
    }
    Ok(start..end)
}

impl From<&ConflictHunk> for v1::ConflictHunk {
    /// SPEC 8.3 conflict hunk line ranges.
    fn from(h: &ConflictHunk) -> Self {
        v1::ConflictHunk {
            base_start: h.base.start,
            base_end: h.base.end,
            ours_start: h.ours.start,
            ours_end: h.ours.end,
            theirs_start: h.theirs.start,
            theirs_end: h.theirs.end,
        }
    }
}

impl TryFrom<&v1::ConflictHunk> for ConflictHunk {
    type Error = ScionError;
    /// SPEC 8.3 conflict hunk; every range must have `end >= start`.
    fn try_from(h: &v1::ConflictHunk) -> Result<Self> {
        Ok(ConflictHunk {
            base: range_u32(h.base_start, h.base_end, "hunk.base")?,
            ours: range_u32(h.ours_start, h.ours_end, "hunk.ours")?,
            theirs: range_u32(h.theirs_start, h.theirs_end, "hunk.theirs")?,
        })
    }
}

impl From<&Conflict> for v1::ConflictMsg {
    /// SPEC 8.3 conflict (kind as its snake_case string).
    fn from(c: &Conflict) -> Self {
        v1::ConflictMsg {
            path: c.path.clone(),
            kind: c.kind.as_str().to_owned(),
            base: opt_entry_to_proto(c.base.as_ref()),
            ours: opt_entry_to_proto(c.ours.as_ref()),
            theirs: opt_entry_to_proto(c.theirs.as_ref()),
            hunks: c.hunks.iter().map(v1::ConflictHunk::from).collect(),
        }
    }
}

impl TryFrom<&v1::ConflictMsg> for Conflict {
    type Error = ScionError;
    /// SPEC 8.3 conflict; an unknown kind string is INVALID_ARGUMENT.
    fn try_from(m: &v1::ConflictMsg) -> Result<Self> {
        if m.kind.is_empty() {
            return Err(missing_field("conflict.kind"));
        }
        let kind = ConflictKind::parse(&m.kind)
            .ok_or_else(|| invalid_value("conflict.kind", format!("unknown kind {:?}", m.kind)))?;
        Ok(Conflict {
            path: m.path.clone(),
            kind,
            base: opt_entry_from_proto(m.base.as_ref())?,
            ours: opt_entry_from_proto(m.ours.as_ref())?,
            theirs: opt_entry_from_proto(m.theirs.as_ref())?,
            hunks: m
                .hunks
                .iter()
                .map(ConflictHunk::try_from)
                .collect::<Result<_>>()?,
        })
    }
}

impl From<&CleanEntry> for v1::CleanEntry {
    /// SPEC 8.3 clean entry (absent entry = deleted).
    fn from(c: &CleanEntry) -> Self {
        v1::CleanEntry {
            path: c.path.clone(),
            entry: opt_entry_to_proto(c.entry.as_ref()),
        }
    }
}

impl TryFrom<&v1::CleanEntry> for CleanEntry {
    type Error = ScionError;
    /// SPEC 8.3 clean entry.
    fn try_from(m: &v1::CleanEntry) -> Result<Self> {
        Ok(CleanEntry {
            path: m.path.clone(),
            entry: opt_entry_from_proto(m.entry.as_ref())?,
        })
    }
}

impl From<&ConflictReport> for v1::ConflictReport {
    /// SPEC 8.3 MERGE_CONFLICT payload.
    fn from(r: &ConflictReport) -> Self {
        v1::ConflictReport {
            clean_entries: r.clean_entries.iter().map(v1::CleanEntry::from).collect(),
            conflicts: r.conflicts.iter().map(v1::ConflictMsg::from).collect(),
        }
    }
}

impl TryFrom<&v1::ConflictReport> for ConflictReport {
    type Error = ScionError;
    /// SPEC 8.3 MERGE_CONFLICT payload.
    fn try_from(m: &v1::ConflictReport) -> Result<Self> {
        Ok(ConflictReport {
            clean_entries: m
                .clean_entries
                .iter()
                .map(CleanEntry::try_from)
                .collect::<Result<_>>()?,
            conflicts: m
                .conflicts
                .iter()
                .map(Conflict::try_from)
                .collect::<Result<_>>()?,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Error detail (SPEC 7.2)
// ---------------------------------------------------------------------------------------------

/// `ErrorDetail.payload` to the wire (SPEC 7.2); `None` for [`ErrorPayload::None`].
pub fn payload_to_proto(p: &ErrorPayload) -> Option<v1::ErrorPayload> {
    use v1::error_payload::Kind;
    let kind = match p {
        ErrorPayload::None => return None,
        ErrorPayload::Subcode(s) => Kind::Subcode(s.clone()),
        ErrorPayload::Limit {
            name,
            limit,
            actual,
        } => Kind::Limit(v1::LimitPayload {
            name: name.clone(),
            limit: *limit,
            actual: *actual,
        }),
        ErrorPayload::MergeConflict(r) => Kind::MergeConflict(v1::ConflictReport::from(&**r)),
        ErrorPayload::RefMismatch {
            current_version,
            current_target,
        } => Kind::RefMismatch(v1::RefMismatch {
            current_version: *current_version,
            current_target: opt_id_bytes(current_target.as_ref()),
        }),
        ErrorPayload::AlreadyLanded {
            source,
            result,
            version,
        } => Kind::AlreadyLanded(v1::AlreadyLandedPayload {
            source: id_bytes(source),
            result: id_bytes(result),
            version: *version,
        }),
        ErrorPayload::Candidates(ids) => Kind::Candidates(v1::Candidates {
            ids: ids_bytes(ids),
        }),
    };
    Some(v1::ErrorPayload { kind: Some(kind) })
}

/// `ErrorDetail.payload` from the wire (SPEC 7.2); absent is [`ErrorPayload::None`].
pub fn payload_from_proto(p: Option<&v1::ErrorPayload>) -> Result<ErrorPayload> {
    use v1::error_payload::Kind;
    let Some(kind) = p.and_then(|p| p.kind.as_ref()) else {
        return Ok(ErrorPayload::None);
    };
    Ok(match kind {
        Kind::Subcode(s) => ErrorPayload::Subcode(s.clone()),
        Kind::Limit(l) => ErrorPayload::Limit {
            name: l.name.clone(),
            limit: l.limit,
            actual: l.actual,
        },
        Kind::MergeConflict(r) => ErrorPayload::MergeConflict(Box::new(r.try_into()?)),
        Kind::RefMismatch(r) => ErrorPayload::RefMismatch {
            current_version: r.current_version,
            current_target: optional_id(&r.current_target, "ref_mismatch.current_target")?,
        },
        Kind::AlreadyLanded(a) => ErrorPayload::AlreadyLanded {
            source: required_id(&a.source, "already_landed.source")?,
            result: required_id(&a.result, "already_landed.result")?,
            version: a.version,
        },
        Kind::Candidates(c) => ErrorPayload::Candidates(parse_ids(&c.ids, "candidates.ids")?),
    })
}

impl From<&ScionError> for v1::ErrorDetail {
    /// SPEC 7.2 `ErrorDetail { code, retryable, message, payload }`.
    fn from(e: &ScionError) -> Self {
        v1::ErrorDetail {
            code: e.code.as_str().to_owned(),
            retryable: e.code.retryable(),
            message: e.message.clone(),
            payload: payload_to_proto(&e.payload),
        }
    }
}

impl TryFrom<&v1::ErrorDetail> for ScionError {
    type Error = ScionError;
    /// SPEC 7.2: an unknown `code` string or a malformed payload is INVALID_ARGUMENT. The
    /// `retryable` flag is derived from the code, never trusted from the wire.
    fn try_from(d: &v1::ErrorDetail) -> Result<Self> {
        let code = ErrorCode::parse(&d.code).ok_or_else(|| {
            invalid_value("error_detail.code", format!("unknown code {:?}", d.code))
        })?;
        Ok(ScionError {
            code,
            message: d.message.clone(),
            payload: payload_from_proto(d.payload.as_ref())?,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// ServerInfo pieces (SPEC 7.3)
// ---------------------------------------------------------------------------------------------

impl From<&Limits> for v1::LimitsMsg {
    /// SPEC 6.5 limits published by GetServerInfo.
    fn from(l: &Limits) -> Self {
        v1::LimitsMsg {
            max_blob_bytes: l.max_blob_bytes,
            max_delta_entries: l.max_delta_entries,
            max_parents: l.max_parents,
            max_message_bytes: l.max_message_bytes,
            merge_max_text_bytes: l.merge_max_text_bytes,
        }
    }
}

impl From<Limits> for v1::LimitsMsg {
    /// SPEC 6.5 limits published by GetServerInfo.
    fn from(l: Limits) -> Self {
        v1::LimitsMsg::from(&l)
    }
}

impl From<&v1::LimitsMsg> for Limits {
    /// SPEC 6.5 limits published by GetServerInfo.
    fn from(l: &v1::LimitsMsg) -> Self {
        Limits {
            max_blob_bytes: l.max_blob_bytes,
            max_delta_entries: l.max_delta_entries,
            max_parents: l.max_parents,
            max_message_bytes: l.max_message_bytes,
            merge_max_text_bytes: l.merge_max_text_bytes,
        }
    }
}

impl From<v1::LimitsMsg> for Limits {
    /// SPEC 6.5 limits published by GetServerInfo.
    fn from(l: v1::LimitsMsg) -> Self {
        Limits::from(&l)
    }
}

/// A token public key `(kid, ed25519 public key)` to the wire (SPEC 7.3, 12.1).
pub fn public_key_to_proto(kid: &str, key: &[u8; 32]) -> v1::PublicKey {
    v1::PublicKey {
        kid: kid.to_owned(),
        ed25519: key.to_vec(),
    }
}

/// A token public key from the wire (SPEC 7.3, 12.1): `kid` and a 32-byte key are required.
pub fn public_key_from_proto(m: &v1::PublicKey) -> Result<(String, [u8; 32])> {
    if m.kid.is_empty() {
        return Err(missing_field("token_public_keys.kid"));
    }
    if m.ed25519.is_empty() {
        return Err(missing_field("token_public_keys.ed25519"));
    }
    let key: [u8; 32] = m.ed25519.as_slice().try_into().map_err(|_| {
        invalid_value(
            "token_public_keys.ed25519",
            format!("expected 32 bytes, got {}", m.ed25519.len()),
        )
    })?;
    Ok((m.kid.clone(), key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_core::Kind;

    fn oid(n: u8, kind: Kind) -> ObjectId {
        ObjectId::new(kind, [n; 32])
    }

    #[test]
    fn id_errors() {
        let e = required_id(&[], "a").unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert_eq!(e.subcode(), Some(subcode::MISSING_FIELD));
        for bad in [vec![1u8; 33], vec![2u8; 34], {
            let mut v = id_bytes(&oid(1, Kind::Blob));
            v[1] = 0x09;
            v
        }] {
            let e = required_id(&bad, "a").unwrap_err();
            assert_eq!(e.subcode(), Some(subcode::BAD_ID_TEXT), "{bad:?}");
        }
        assert_eq!(optional_id(&[], "x").unwrap(), None);
        let id = oid(7, Kind::Changeset);
        assert_eq!(required_id(&id_bytes(&id), "x").unwrap(), id);
        let e = required_change_id(&[1, 2, 3], "change_id").unwrap_err();
        assert_eq!(e.subcode(), Some(subcode::BAD_ID_TEXT));
        let e = required_change_id(&[], "change_id").unwrap_err();
        assert_eq!(e.subcode(), Some(subcode::MISSING_FIELD));
    }

    #[test]
    fn modes() {
        assert_eq!(parse_mode(4).unwrap(), Mode::Tree);
        for bad in [0, 5, 256, 257, u32::MAX] {
            assert_eq!(
                parse_mode(bad).unwrap_err().subcode(),
                Some(subcode::INVALID_MODE)
            );
        }
    }

    #[test]
    fn entry_round_trip() {
        let e = EntryInfo {
            mode: Mode::Executable,
            id: oid(3, Kind::Blob),
            size: 99,
        };
        let m = v1::EntryInfo::from(&e);
        assert_eq!(EntryInfo::try_from(&m).unwrap(), e);
        let missing = v1::EntryInfo {
            mode: 1,
            id: vec![],
            size: 0,
        };
        assert_eq!(
            EntryInfo::try_from(&missing).unwrap_err().subcode(),
            Some(subcode::MISSING_FIELD)
        );
    }

    #[test]
    fn delta_errors_before_path_parsing() {
        // These fail before RepoPath::parse is reached.
        let m = v1::DeltaEntryMsg {
            path: b"a".to_vec(),
            op: 0,
            ..Default::default()
        };
        assert_eq!(
            DeltaEntry::try_from(&m).unwrap_err().subcode(),
            Some(subcode::MISSING_FIELD)
        );
        // An unknown op is a plain INVALID_ARGUMENT, not a missing field.
        let m = v1::DeltaEntryMsg {
            path: b"a".to_vec(),
            op: 3,
            ..Default::default()
        };
        let e = DeltaEntry::try_from(&m).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert_eq!(e.subcode(), None);
        let m = v1::DeltaEntryMsg {
            path: b"a".to_vec(),
            op: DELTA_OP_PUT,
            mode: 4,
            id: id_bytes(&oid(1, Kind::Blob)),
            size: 0,
        };
        assert_eq!(
            DeltaEntry::try_from(&m).unwrap_err().subcode(),
            Some(subcode::INVALID_MODE)
        );
        let m = v1::DeltaEntryMsg {
            path: b"a".to_vec(),
            op: DELTA_OP_PUT,
            mode: 1,
            id: vec![],
            size: 0,
        };
        assert_eq!(
            DeltaEntry::try_from(&m).unwrap_err().subcode(),
            Some(subcode::MISSING_FIELD)
        );
    }

    #[test]
    fn delta_to_proto_shape() {
        let d = DeltaEntry {
            path: RepoPath::root(),
            op: DeltaOp::Delete,
        };
        let m = v1::DeltaEntryMsg::from(&d);
        assert_eq!(m.op, DELTA_OP_DELETE);
        assert!(m.id.is_empty());
        let d = DeltaEntry {
            path: RepoPath::root(),
            op: DeltaOp::Put {
                mode: Mode::Symlink,
                id: oid(5, Kind::Blob),
                size: 12,
            },
        };
        let m = v1::DeltaEntryMsg::from(&d);
        assert_eq!((m.op, m.mode, m.size), (DELTA_OP_PUT, 3, 12));
        assert_eq!(m.id, id_bytes(&oid(5, Kind::Blob)));
    }

    #[test]
    fn empty_extensions_round_trip() {
        let x = Extensions::new();
        assert!(extensions_to_proto(&x).is_empty());
        assert_eq!(extensions_from_proto(&[]).unwrap(), x);
        // Ordering is checked before any insert happens (insert is P01 code).
        let unsorted = [
            v1::Extension {
                tag: 4,
                value: vec![],
            },
            v1::Extension {
                tag: 2,
                value: vec![],
            },
        ];
        let dup = [
            v1::Extension {
                tag: 2,
                value: vec![],
            },
            v1::Extension {
                tag: 2,
                value: vec![],
            },
        ];
        assert_eq!(
            extensions_from_proto(&unsorted).unwrap_err().subcode(),
            Some(subcode::UNSORTED)
        );
        assert_eq!(
            extensions_from_proto(&dup).unwrap_err().subcode(),
            Some(subcode::DUPLICATE)
        );
    }

    #[test]
    fn limits_and_keys() {
        let l = Limits::default();
        assert_eq!(Limits::from(&v1::LimitsMsg::from(&l)), l);
        let k = public_key_to_proto("abcd", &[9; 32]);
        assert_eq!(
            public_key_from_proto(&k).unwrap(),
            ("abcd".to_owned(), [9; 32])
        );
        let short = v1::PublicKey {
            kid: "k".into(),
            ed25519: vec![1; 31],
        };
        assert_eq!(
            public_key_from_proto(&short).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn conflict_kind_unknown() {
        let m = v1::ConflictMsg {
            path: b"p".to_vec(),
            kind: "weird".into(),
            ..Default::default()
        };
        assert_eq!(
            Conflict::try_from(&m).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let h = v1::ConflictHunk {
            base_start: 3,
            base_end: 2,
            ..Default::default()
        };
        assert!(ConflictHunk::try_from(&h).is_err());
    }

    #[test]
    fn error_detail_unknown_code() {
        let d = v1::ErrorDetail {
            code: "NOPE".into(),
            ..Default::default()
        };
        assert!(ScionError::try_from(&d).is_err());
    }
}
