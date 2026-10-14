//! `data` payloads of the `--json` envelopes (SPEC 11.8 "every command supports `--json` with a
//! documented, versioned schema"; PLAN 6.15).
//!
//! Every id is the full text form (`c1<64 hex>`, `b1...`, `m1...`); paths are lossy UTF-8 of the
//! repository path bytes. Human output uses short ids instead (`c1` + 12 hex).

use schemars::JsonSchema;
use serde::Serialize;

pub use scion_wc::{FileChangeView, GotoReport, LandReport};

/// `scion login` (SPEC 12.2, 12.3, 17).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct LoginData {
    /// The principal the server issued the token for (`github:<login>` or `service:<name>`).
    pub principal: String,
    /// Token expiry, Unix seconds.
    pub expires_at_unix: i64,
    /// `github` (device flow) or `service`.
    pub method: String,
    /// Where the credentials were written (mode 0600).
    pub credentials_path: String,
    /// Whether the GitHub token was stored for silent re-exchange (SPEC 12.2).
    pub github_token_stored: bool,
}

/// `scion whoami` (SPEC 7.7).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct WhoamiData {
    /// Authenticated principal.
    pub principal: String,
    /// Its groups under the current policy, sorted.
    pub groups: Vec<String>,
    /// Token expiry, Unix seconds.
    pub expires_at_unix: i64,
    /// The endpoint that answered.
    pub endpoint: String,
}

/// One case collision: the second path was not written (SPEC 4.4; PLAN 6.17).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct CollisionView {
    /// Path written.
    pub written: String,
    /// Path skipped.
    pub skipped: String,
}

/// `scion checkout` (SPEC 11.3).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct CheckoutData {
    /// Working-copy root (absolute).
    pub root: String,
    /// The checked-out changeset.
    pub base: String,
    /// Tracked ref, when the revision was a ref name.
    pub tracking_ref: Option<String>,
    /// Sparse profile (`""` is the whole tree).
    pub sparse: Vec<String>,
    /// Inaccessible subtrees recorded at checkout.
    pub inaccessible: Vec<String>,
    /// Case collisions that were reported instead of written.
    pub collisions: Vec<CollisionView>,
}

/// `scion sparse add|remove` (SPEC 11.2).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct SparseData {
    /// `add` or `remove`.
    pub action: String,
    /// The prefixes given.
    pub paths: Vec<String>,
    /// The resulting profile.
    pub sparse: Vec<String>,
}

/// `scion status` (SPEC 11.5).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct StatusData {
    /// Base changeset.
    pub base: String,
    /// Tracked ref.
    pub tracking_ref: Option<String>,
    /// Modified files.
    pub modified: Vec<String>,
    /// Untracked, not ignored, in-scope files.
    pub added: Vec<String>,
    /// Deleted files.
    pub deleted: Vec<String>,
    /// Files whose mode changed.
    pub mode_changed: Vec<String>,
    /// Dirty files whose base content is redacted (SPEC 13.7).
    pub redacted_dirty: Vec<String>,
    /// Files re-hashed by this status.
    pub hashed: u64,
    /// Kind of the pending operation, if any (`rebase`, `commit`, ...).
    pub pending: Option<String>,
    /// Unresolved conflict paths of a paused rebase.
    pub conflicts: Vec<String>,
    /// No changes at all.
    pub clean: bool,
}

/// `scion diff` (SPEC 11.5): working-copy changes against the base.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct DiffData {
    /// Base changeset.
    pub base: String,
    /// Changed files, sorted by path.
    pub changes: Vec<FileChangeView>,
}

/// One delta entry (SPEC 4.3.4).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct DeltaView {
    /// Path.
    pub path: String,
    /// `put` or `delete`.
    pub op: String,
    /// Mode of a put (1 file, 2 executable, 3 symlink).
    pub mode: Option<u8>,
    /// Blob of a put.
    pub id: Option<String>,
    /// Size of a put.
    pub size: Option<u64>,
}

/// `scion commit` and `scion amend` (SPEC 11.6, 11.7).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct CommitData {
    /// The created changeset (the new base).
    pub changeset: String,
    /// Its root tree.
    pub root_tree: String,
    /// Its change id (32 hex).
    pub change_id: String,
    /// Its parents.
    pub parents: Vec<String>,
    /// The delta sent in CreateChangeset.
    pub delta: Vec<DeltaView>,
    /// Blobs uploaded (only those the server was missing).
    pub uploaded: u64,
    /// The base after the command (differs from `changeset` when a rebase continued).
    pub base: String,
    /// Pending operation left after the command (a continued rebase may pause again).
    pub pending: Option<String>,
}

/// `scion rebase --onto` / `--abort` (SPEC 11.7).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RebaseData {
    /// Whether this was `--abort`.
    pub aborted: bool,
    /// Base after the command.
    pub base: String,
    /// Destination (`--onto`).
    pub onto: Option<String>,
    /// Rebased results in stack order.
    pub rebased: Vec<String>,
    /// Conflicting paths (non-empty: exit code 3).
    pub conflicts: Vec<String>,
    /// New head when the rebase completed.
    pub head: Option<String>,
}

/// `scion resolve` (SPEC 11.7).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ResolveData {
    /// The resolved path.
    pub path: String,
    /// Conflicts still unresolved.
    pub remaining: Vec<String>,
}

/// A Meta as shown to clients (SPEC 4.3.3, 13.4): when `redacted`, only `meta_id` is set and
/// humans see `<redacted>`.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct MetaJson {
    /// The Meta is redacted.
    pub redacted: bool,
    /// Meta object id.
    pub meta_id: String,
    /// Author.
    pub author: Option<String>,
    /// Author time.
    pub author_time: Option<TimeView>,
    /// Committer.
    pub committer: Option<String>,
    /// Committer time.
    pub committer_time: Option<TimeView>,
    /// Creator principal.
    pub creator: Option<String>,
    /// Message.
    pub message: Option<String>,
}

/// A timestamp (SPEC 4.3.3).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct TimeView {
    /// Unix seconds.
    pub seconds: i64,
    /// Time-zone offset in minutes.
    pub tz_offset_minutes: i64,
}

/// One `scion log` entry (SPEC 7.4, 12.6).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct LogEntryView {
    /// Changeset id.
    pub id: String,
    /// Parents.
    pub parents: Vec<String>,
    /// Root tree.
    pub root_tree: String,
    /// Change id (32 hex).
    pub change_id: String,
    /// Delta entries outside the caller's read scope were removed (SPEC 12.6).
    pub filtered: bool,
    /// Meta.
    pub meta: MetaJson,
}

/// `scion log` (SPEC 7.4).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct LogData {
    /// Starting changeset.
    pub from: String,
    /// Path filter.
    pub path: Option<String>,
    /// First-parent only.
    pub first_parent: bool,
    /// Entries, newest first.
    pub entries: Vec<LogEntryView>,
    /// More entries exist beyond `--limit`.
    pub truncated: bool,
}

/// One blame range (SPEC 9.2); lines are 0-based.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct BlameRangeView {
    /// First line (0-based) in the blamed file.
    pub start_line: u32,
    /// Number of lines.
    pub line_count: u32,
    /// Changeset that introduced the lines.
    pub origin_changeset: String,
    /// First line (0-based) in the origin's version.
    pub origin_start_line: u32,
}

/// `scion blame` (SPEC 9.2).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct BlameData {
    /// Blamed changeset.
    pub changeset: String,
    /// Path.
    pub path: String,
    /// Ranges in line order.
    pub ranges: Vec<BlameRangeView>,
}

/// One unified hunk (SPEC 7.4 `include_content_hunks`).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct HunkView {
    /// First old line (1-based).
    pub old_start: u32,
    /// Old line count.
    pub old_lines: u32,
    /// First new line (1-based).
    pub new_start: u32,
    /// New line count.
    pub new_lines: u32,
    /// Lines, each starting with ` `, `-` or `+` (lossy UTF-8).
    pub lines: Vec<String>,
}

/// One file changed by a changeset (SPEC 4.3.4).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ShowChange {
    /// Path.
    pub path: String,
    /// `added`, `deleted`, `modified` or `mode_changed`.
    pub status: String,
    /// Old blob.
    pub old_id: Option<String>,
    /// New blob.
    pub new_id: Option<String>,
    /// Old mode.
    pub old_mode: Option<u8>,
    /// New mode.
    pub new_mode: Option<u8>,
    /// Treated as binary (no hunks).
    pub binary: bool,
    /// Content hunks.
    pub hunks: Vec<HunkView>,
}

/// `scion show` (SPEC 11.8, 13.4).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ShowData {
    /// Changeset id.
    pub id: String,
    /// Parents.
    pub parents: Vec<String>,
    /// Root tree.
    pub root_tree: String,
    /// Change id (32 hex).
    pub change_id: String,
    /// Filtered view (SPEC 12.6).
    pub filtered: bool,
    /// Meta (`<redacted>` for humans when redacted).
    pub meta: MetaJson,
    /// Changes against the first parent (the empty tree for a root).
    pub changes: Vec<ShowChange>,
}

/// `scion cat <rev>:<path>` (SPEC 7.4 ReadPath).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct CatData {
    /// Changeset read.
    pub changeset: String,
    /// Path.
    pub path: String,
    /// Blob id.
    pub id: String,
    /// Mode (1 file, 2 executable, 3 symlink).
    pub mode: u8,
    /// Size in bytes.
    pub size: u64,
    /// `utf8` or `hex`: how `content` is encoded.
    pub encoding: String,
    /// Content.
    pub content: String,
}

/// One object targeted by `scion redact` (SPEC 13.2 tooling).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RedactTarget {
    /// Object id.
    pub id: String,
    /// `blob` or `meta`.
    pub kind: String,
}

/// `scion redact` (SPEC 13.2).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct RedactData {
    /// Redacted objects (for `--all-versions`, every distinct blob the path ever held).
    pub targets: Vec<RedactTarget>,
    /// Audit reason.
    pub reason: String,
    /// Redaction epoch after the command.
    pub epoch: u64,
    /// No server serves the objects after this time (Unix ms).
    pub effective_by_unix_ms: i64,
    /// `--purge-now` purged the objects irreversibly.
    pub purged: bool,
}

/// Error envelope (`scion.cli.error.v1`), printed on stdout under `--json` when a command
/// fails (PLAN 6.15).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ErrorData {
    /// SPEC 7.2 error code (`NOT_FOUND`, ...), or `USAGE` / `LOCAL` for client-side errors.
    pub code: String,
    /// Subcode, when the error carries one.
    pub subcode: Option<String>,
    /// Message.
    pub message: String,
    /// Process exit code (PLAN 6.15).
    pub exit_code: i32,
}
