# Contract: storage semantics (normative)

Status: normative. Owner: P00 (frozen). Implementations: `scion-store` (`MemMetaStore`,
`MemBlobStore`, `ObjectRepo`; P03), `scion-store-crdb` and `scion-store-s3` (P15). The shared
suites `scion_store::conformance::{meta_store_suite, blob_store_suite}` test this document.
SPEC references: 4.5, 5.1-5.4, 8.4, 8.5, 9.1, 13.2, 18.8; PLAN 2.3, 3.2, 6.8.

## 1. General rules

1. **One method = one transaction.** Every `MetaStore` method is exactly one serializable
   (CockroachDB `SERIALIZABLE`) transaction. It either commits all of its effects or none. The
   only exception is the informational read in `cas_ref`'s `LandedConflict` path (section 4.4),
   which reads an immutable row after the rollback.
2. **Infrastructure failures** surface as `ScionError` UNAVAILABLE with subcode
   `store_unavailable` (connection or backend down), `txn_retry` (CockroachDB `40001` retries
   exhausted, or a Mem forced failure) or `injected` / `injected_crash` (fault decorators). A
   failed call has no effects, except for fail-after faults (`meta.<method>.after` armed with
   `FaultAction::FailAfter`), which apply the effect and then report UNAVAILABLE/injected
   ("committed, response lost").
3. **Idempotent inserts.** Every insert of an identity-bearing row uses `ON CONFLICT DO NOTHING`;
   an existing row is never modified by an insert, and the outcome is
   `InsertOutcome::AlreadyExists`.
4. **Ordering.** Methods returning lists return them in the order stated; "ascending" means
   bytewise ascending on the stated key (ids compare as their 34 raw bytes, paths bytewise).
5. **Repository id.** `repo_id = i64::from_be_bytes(BLAKE3(repo_name)[0..8]) & i64::MAX`. It is
   deterministic, so every server that loads the same configuration agrees without coordination.
   `MetaStore::repo_id()` returns it without IO. Every table is keyed by `repo_id` first.
6. **Times** are `SystemTime` supplied by the caller (from `Clock`), stored as `TIMESTAMPTZ`
   with microsecond precision. Callers must not depend on sub-microsecond round trips.
7. **Consistency.** `Consistency::Strong` reads are linearizable. `Consistency::Stale` reads may
   omit rows committed recently (CockroachDB `AS OF SYSTEM TIME follower_read_timestamp()`;
   Mem: a snapshot `stale_lag_commits` commits old). Refs and the redaction epoch are always read
   strongly. Callers retry a stale miss strongly before reporting NOT_FOUND (SPEC 5.4).

## 2. Schema (CockroachDB DDL shape)

The authoritative DDL is `scion_store_crdb::MIGRATIONS`. Shape (all `PRIMARY KEY`s start with
`repo_id`):

| Table | Primary key | Columns |
|---|---|---|
| `repos` | `repo_id` | `name UNIQUE`, `created_at` |
| `objects` | `(repo_id, object_id)` | `kind INT2`, `size`, `stored_size`, `inline_data BYTES NULL`, `state STRING ('live'/'quarantined'/'purged')`, `created_at` |
| `object_uploads` | `(repo_id, object_id, principal)` | `at` |
| `object_paths` | `(repo_id, object_id, path)` | `first_changeset`, `at` |
| `changesets` | `(repo_id, changeset_id)` | `encoded`, `generation`, `skip BYTES[]`, `change_id`, `created_by`, `created_at` |
| `changeset_children` | `(repo_id, parent_id, child_id)` | |
| `refs` | `(repo_id, name)` | `target`, `version`, `updated_at` |
| `ref_log` | `(repo_id, name, version)` | `old_target NULL`, `new_target NULL`, `principal`, `at`, `landed BYTES` (canonical list of `(change_id 16, source 34, result 34)`) |
| `landed` | `(repo_id, ref, change_id)` | `version`, `source_changeset`, `result_changeset` |
| `derived` | `(repo_id, type, type_version, key)` | `value NULL`, `storage_key NULL`, `source_object NULL`, `at` |
| `derive_leases` | `(repo_id, type, type_version, key)` | `holder`, `expires_at` |
| `redactions` | `(repo_id, object_id)` | `state ('quarantined'/'purged')`, `reason`, `actor`, `created_at`, `purge_after`, `epoch` |
| `counters` | `(repo_id, name)` | `value` (`redaction_epoch`) |
| `audit_log` (PLAN 6.8) | `(repo_id, seq)` | `at`, `actor`, `action`, `target`, `reason`, `detail` |

`derived.source_object` is an implementation column that does not participate in identity
(allowed by SPEC 5.2); it keys content-bearing records by their source object (SPEC 9.1).

Secondary indexes:

- **Changeset id prefix:** served by a range scan on the `changesets` primary key: the 64-hex
  prefix `p` maps to the byte range `[01 03 || lo(p), 01 03 || hi(p))` where `lo` pads `p` with
  `0` and `hi` is the next prefix (an odd final nibble is handled by the padding). No extra index
  is needed.
- `derived_by_source ON derived (repo_id, source_object) WHERE source_object IS NOT NULL`.
- `redactions_by_epoch ON redactions (repo_id, epoch)`.
- `redactions_by_purge_after ON redactions (repo_id, purge_after) WHERE state = 'quarantined'`.

`migrate()` is idempotent (`CREATE ... IF NOT EXISTS`). `connect()` ensures the `repos` row with
`INSERT ... ON CONFLICT DO NOTHING`.

## 3. CockroachDB transaction discipline

- Each method runs `BEGIN ISOLATION LEVEL SERIALIZABLE; ...; COMMIT` on a pooled connection
  (deadpool-postgres). SQLSTATE `40001` (and `40003` ambiguous-result on read-only methods) is
  retried client-side with full-jitter backoff, at most 10 attempts; exhaustion is
  UNAVAILABLE/txn_retry.
- Connection and pool errors are UNAVAILABLE/store_unavailable.
- `Consistency::Stale` reads run as a single statement with
  `AS OF SYSTEM TIME follower_read_timestamp()` outside an explicit transaction.
- Unique violations other than those mapped below (`23505` on `landed`) are bugs and surface as
  UNAVAILABLE/store_unavailable with an ERROR log event.

## 4. Method contracts

For each method: reads, writes, conflict behaviour, outcome, SQL shape, Mem notes.

### 4.1 Objects

**`ping()`** - `SELECT 1`. Mem: succeeds unless a forced failure applies.

**`get_objects(ids, c)`** - Reads `objects` rows for `ids`; returns one entry per input id in
input order (`None` when absent; duplicates allowed). SQL: `SELECT ... FROM objects WHERE
repo_id = $1 AND object_id = ANY($2)`, reordered client-side.

**`insert_object(row)`** - `INSERT INTO objects ... ON CONFLICT (repo_id, object_id) DO
NOTHING`. Returns `Inserted` or `AlreadyExists`. An existing row is never changed (in particular
a quarantined or purged row stays so; `ObjectRepo` rejects such ids before calling).

**`record_upload(id, principal, at)`** - `INSERT INTO object_uploads ... ON CONFLICT DO
NOTHING` (the first `at` is kept).

**`has_upload(id, principal)`** - Point read on `object_uploads`.

**`object_paths(id, limit)`** - Rows of THIS id only: `SELECT ... WHERE repo_id = $1 AND
object_id = $2 ORDER BY path LIMIT $3`.

### 4.2 Changesets

**`get_changesets(ids, c)`** - As `get_objects`, over `changesets`.

**`changesets_by_prefix(hex_prefix, limit)`** - `hex_prefix` is 0-64 lowercase hex digits of the
digest (callers validate the 8-digit minimum). Returns changeset ids whose digest hex starts with
it, ascending, at most `limit`. SQL: primary-key range scan (section 2).

**`children(parent)`** - `SELECT child_id FROM changeset_children WHERE parent_id = $2 ORDER BY
child_id`.

**`has_root_changeset()`** - True iff any changeset has no parents, i.e. `generation = 1`.
SQL: `SELECT EXISTS (SELECT 1 FROM changesets WHERE repo_id = $1 AND generation = 1)`.

**`commit_changeset(bundle)`** - SPEC 5.3 step 2, one transaction:
1. `INSERT INTO changesets ... ON CONFLICT DO NOTHING` - its outcome is the method's outcome.
2. `INSERT INTO changeset_children (parent, child)` for every parent, `ON CONFLICT DO NOTHING`.
3. `INSERT INTO object_paths` for every row, `ON CONFLICT DO NOTHING`.
4. `INSERT INTO derived` for every eager row, `ON CONFLICT DO NOTHING`.

Each insert is independent, so re-committing a bundle whose changeset row exists still fills in
any missing child/path/derived rows (crash recovery) and returns `AlreadyExists`. The caller
guarantees that every referenced object already exists (SPEC 4.5 invariant 2).

### 4.3 Refs

**`get_ref(name)`**, **`list_refs()`** - Strong reads (`list_refs` ascending by name).

**`cas_ref(cas)`** - SPEC 5.3 step 3, 8.4, 8.5. One transaction:
1. `SELECT target, version FROM refs WHERE repo_id = $1 AND name = $2 FOR UPDATE`.
2. Check `cas.expect`: `Absent` requires no row; `Version(v)` requires a row with version `v`.
   On failure: return `VersionMismatch { current }` with no writes.
3. Compute the new version:
   - `Version(v)` -> `v + 1`.
   - `Absent` -> `1 + max(ref_log.version for this name)` (`1` for a name that never existed).
     Re-creating a deleted ref therefore continues its numbering, keeping `<ref>@<version>`
     unambiguous and the `ref_log` key unique.
4. Apply: `Absent` + `Some(t)` inserts the ref; `Version(v)` + `Some(t)` updates
   `SET target = t, version = v + 1, updated_at = at WHERE version = v`; `Version(v)` + `None`
   deletes the ref. `Absent` + `None` is INVALID_ARGUMENT.
5. Insert the `ref_log` row `(name, new_version, old_target, new_target, principal, at, landed)`.
6. Insert one `landed` row per `cas.landed` entry with `version = new_version`. A unique
   violation (an existing `(ref, change_id)`) rolls back the WHOLE transaction (nothing from
   steps 4-7 persists) and the method returns `LandedConflict { existing }`, where `existing` is
   the conflicting row read strongly after the rollback (landed rows are immutable).
7. Insert `cas.audit` into `audit_log` when present.
8. Return `Committed(Some(new_row))`, or `Committed(None)` after a delete.

Mem: the version check, the landed uniqueness check and all writes happen under the single mutex;
a landed conflict is detected before any write.

**`ref_log(name, from_version, limit)`** - Entries with `version >= from_version`, ascending,
at most `limit`.

**`get_landed(name, change_id)`** - Point read on `landed`.

### 4.4 Derived data and leases

**`get_derived(k, c)`** - Point read on `derived`.

**`put_derived(rows)`** - `INSERT ... ON CONFLICT DO NOTHING` for every row (lazy write-back is
idempotent and deterministic, SPEC 4.5 invariant 3).

**`delete_derived_by_source(types, source)`** - `DELETE FROM derived WHERE repo_id = $1 AND
source_object = $2 AND type = ANY($3)`; returns the number of rows deleted (SPEC 13.5).

**`try_lease(k, holder, ttl, now)`** - Takes or renews the lease when no lease row exists, the
existing one has `expires_at <= now`, or it is held by `holder`; then
`expires_at = now + ttl` and the result is `true`. Otherwise `false`, no writes. Leases only
deduplicate work; correctness never depends on them (SPEC 9.1). Decorators count `lease_calls`
and emit a `scion::lock` span.

**`release_lease(k, holder)`** - `DELETE ... WHERE holder = $holder`; releasing a lease not held
is a no-op.

### 4.5 Redaction (SPEC 13.2, 18.8)

**`redact(t)`** - One transaction:
1. Every id in `t.ids` must have an `objects` row, else NOT_FOUND and no effects. (Kind checks,
   Blob/Meta only, are the Redactor's job.)
2. `epoch = counters.redaction_epoch + 1` (row created at 0 when absent), written back.
3. For each id: when its `redactions` row is absent, insert `(Quarantined, reason, actor,
   created_at = at, purge_after, epoch)`; when present and Quarantined, refresh `reason`, `actor`,
   `purge_after` and `epoch`; when Purged, leave it unchanged.
4. Set `objects.state = 'quarantined'` for every id not already purged.
5. Append one `audit_log` row per id (`action = "redact"`, `target` = id text, reason, actor).
6. Return `epoch`.

**`unredact(ids, audit)`** - Every id must have a Quarantined `redactions` row, else
INVALID_ARGUMENT/not_quarantined and no effects (PLAN 6.7: unredact after purge is
not_quarantined). Then: epoch + 1; delete the `redactions` rows; `objects.state = 'live'`;
append `audit`; return the new epoch.

**`mark_purged(ids, audit)`** - For every id: `redactions.state = 'purged'`,
`objects.state = 'purged'`, `objects.inline_data = NULL`. The epoch is unchanged (the object
stays redacted). Append `audit`.

**`redaction_epoch()`** - Strong read of `counters.redaction_epoch` (0 when absent).

**`list_redactions(since_epoch)`** - In one snapshot: rows with `epoch > since_epoch` ordered by
`(epoch, object_id)`, plus the current epoch. Consumer rule (daemons, P16): every epoch
increment is either a `redact` (whose rows carry that epoch) or an `unredact` (whose rows were
deleted). When `current_epoch` exceeds the largest epoch among the returned rows (or rows are
missing for some epochs), the consumer re-lists from `since_epoch = 0` and REPLACES its set, so
unredactions propagate. `since_epoch = 0` always returns the complete current set.

**`due_purges(now, limit)`** - Quarantined ids with `purge_after <= now`, ascending by
`(purge_after, object_id)`, at most `limit`.

### 4.6 Audit (PLAN 6.8)

**`append_audit(row)`** - Insert with a new monotonically increasing `seq`.

**`audit_log(limit)`** - The most recent `limit` rows, returned oldest first (newest last).

## 5. In-memory implementation notes (`MemMetaStore`, `MemBlobStore`)

- All state lives in `im` persistent maps behind ONE `parking_lot::Mutex`; each trait method
  takes the lock once, so the store is strictly serializable and every method is atomic.
- After each committing method the new state root is pushed onto a ring of the last
  `K = max(stale_lag_commits, 1)` snapshots. `Consistency::Stale` reads the snapshot
  `stale_lag_commits` commits old (or the latest when the lag is 0); `advance_stale_view()` makes
  stale reads see the latest state; `set_stale_lag` changes the lag.
- `fail_txn_every = Some(n)`: every n-th method call (counting every async method, reads
  included) fails UNAVAILABLE/txn_retry before taking any effect.
- `MemBlobStore`: a `BTreeMap<String, Bytes>`; `corrupt(key)` flips one bit of the stored value;
  `set_down(true)` makes every operation fail UNAVAILABLE/store_unavailable. `get` of a missing
  key is `Ok(None)`; `delete` and `copy` of a missing key are idempotent (`copy` of a missing
  source is NOT_FOUND).

## 6. BlobStore contract

`put` overwrites; `get` of a missing key is `Ok(None)`; `copy(from, to)` copies bytes (NOT_FOUND
when `from` is missing); `delete` is idempotent; `exists` is a point check; `list(prefix)`
returns keys ascending. Object storage must provide strong read-after-write consistency
(SPEC 3.5). S3 keys are prefixed with `S3Config.key_prefix`.

## 7. ObjectRepo: key layout and write ordering

**Key layout (SPEC 5.1):** `<prefix>/r/<repo_id>/<letter>/<hex[0:2]>/<hex>` for live bytes and
`<prefix>/q/<repo_id>/<letter>/<hex[0:2]>/<hex>` for quarantined bytes. `<repo_id>` is decimal,
`<hex>` the 64 lowercase hex digits of the digest, `<letter>` the kind letter (`b t c m`), and
for derived kinds `x` (kind 0x10) or `x<2-hex kind>` (other derived kinds).

**Where bytes live:**

- Blobs always go to object storage as a zstd frame (level `zstd_level`) of the raw content.
- Trees, Metas and derived objects whose canonical encoding is at most
  `inline_threshold_bytes` are stored inline (`objects.inline_data`, uncompressed canonical
  bytes); larger ones as zstd frames in object storage.
- Changesets are stored only in `changesets.encoded` (written by `commit_changeset`);
  `put_encoded(Kind::Changeset, ..)` is INVALID_ARGUMENT/invalid_kind. `get` of a changeset id
  reads the `changesets` row.
- `stored_size` is the frame length (object storage) or the inline length.

**Write ordering (SPEC 5.3 step 1):**

1. Validate: size against `max_blob_bytes` (TOO_LARGE `max_blob_bytes`), decoded form of known
   kinds (Tree, Meta, HistoryNode), digest (`put_verified`: INTEGRITY on mismatch).
2. Re-introduction blocking: when an `objects` row exists with state quarantined or purged, fail
   REDACTED.
3. Write the bytes (object storage `put`), unless inline. Checkpoint `upload.after_bytes`.
4. `insert_object` with `state = live` (ON CONFLICT DO NOTHING). Checkpoint `upload.after_row`.

A `live` row therefore always implies durable bytes. Bytes without a row are harmless orphans.

**Reads:** `stat` reads stale first and retries a miss strongly (SPEC 5.4). `get` /
`get_frame`: missing row -> NOT_FOUND; state quarantined or purged -> REDACTED; otherwise load the
inline data or the live key, decompress, and verify BLAKE3 against the id. On mismatch: record an
integrity failure in `Counters`, emit an ERROR event `event="integrity_alert"` with the object id,
and return INTEGRITY (PLAN 6.10). Every successful read records an object read by kind and raw
bytes (inline trees count as `trees_read`). Decoded trees, metas and changesets are cached in
bounded LRUs (immutable data), so they keep serving while the metadata store is down.

**Quarantine moves (SPEC 13.2):** `quarantine_bytes` copies live -> quarantine (checkpoint
`quarantine.after_copy`), then deletes the live key; both steps are idempotent, so a crash
between them is completed by re-running. Inline objects have no bytes to move (their inline data
is never served and is cleared by `mark_purged`). `restore_bytes` copies quarantine -> live, then
deletes the quarantine key. `purge_bytes` deletes both keys.

## 8. fsck (SPEC 4.5 oracle)

`fsck` walks every changeset row and every ref and reports `Violation`s: missing root tree,
meta, parents or delta blobs (`MissingObject`), stored bytes not hashing to the id
(`DigestMismatch`, when `verify_bytes`), non-canonical encodings (`BadEncoding`),
`delta != FileDiff(T1, root_tree)` (`DeltaMismatch`), missing required eager rows
(`MissingEager`), wrong generation or skip pointers (`BadGraph`), and refs whose target is
missing (`RefTargetMissing`). Quarantined or purged bytes are not violations.
