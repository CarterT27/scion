# Scion Source Control Specification

Status: Draft v1 (core system; language-agnostic contracts, reference stack noted)

Purpose: Define a server-centric, always-online source control system for a single large monorepo
used concurrently by humans, coding agents, and CI.

Provenance: This is a clean-room specification. Its only external design input is the public
article "Redesigning source control" (anthropic.com/engineering/redesigning-source-control). All
formats, schemas, algorithms, and thresholds below are original to this document. Contributors
MUST NOT introduce material derived from non-public implementations of similar systems.

## Normative Language

The key words `MUST`, `MUST NOT`, `REQUIRED`, `SHOULD`, `SHOULD NOT`, `RECOMMENDED`, `MAY`, and
`OPTIONAL` in this document are to be interpreted as described in RFC 2119.

`Implementation-defined` means the behavior is part of the implementation contract, but this
specification does not prescribe one universal policy. Implementations MUST document the selected
behavior.

`Deferred` marks behavior intentionally out of scope for v1. Every deferred item is listed in
Appendix A, and the v1 design MUST NOT preclude it (see Appendix C).

## 1. Problem Statement

Scion stores and serves the history of one monorepo whose history and working copies are too large,
and whose write rate is too high, for a decentralized full-clone system to serve efficiently.

The system solves five problems:

- Clients get the files they need without cloning history: every read is a request for an
  immutable, content-addressed object that any cache can answer.
- Commits, rebases, merges, and landings run on the server, cost proportional to the size of the
  change (not the repository), and require no global lock or merge queue.
- History queries (log, blame, diff, ancestry) are server APIs, so tools and CI jobs need no local
  repository.
- Working copies are optional and sparse; a client materializes only the directories it asks for.
- Content that must not exist (leaked secrets, legally required removals) can be removed without
  rewriting history or changing any identifier.

Important boundaries:

- Scion is the source of truth for repository content, history, and refs.
- Code review, code search, and CI orchestration are separate systems that consume Scion APIs. The
  only review-related concept in Scion is the stable `ChangeId` (Section 4.3.5).
- Semantic correctness of `main` (does it build, do tests pass) is enforced by post-merge and
  pre-deploy testing outside Scion. Scion guarantees textual merge correctness only.

## 2. Goals and Non-Goals

### 2.1 Goals

- One repository, many concurrent writers (humans, agents, CI), Unix clients (Linux, macOS).
- Immutable, content-addressed object model with self-verifying identifiers.
- Changesets stored as deltas against their first parent; trees computed server-side in
  O(changed paths x depth).
- Lock-free landing onto refs via rebase + single-row compare-and-swap, with idempotent retries.
- A generic, versioned derived-data framework so new history indexes need no client changes.
- Two read tiers (per-node daemon, authoritative server) speaking identical read RPCs.
- Sparse materialized working copies with a correct, stat-cache-based `status`.
- GitHub login for humans; configured service principals for agents and CI.
- Path-prefix access control defined in a config file and applied retroactively.
- Redaction of file contents and changeset metadata without history rewrite.
- One-way git import for migration and differential testing.

### 2.2 Non-Goals

- Offline operation, peer-to-peer sync, or a full local clone.
- Git wire-protocol or on-disk compatibility.
- A global merge queue or any landing serialization beyond the ref CAS.
- Semantic (build/test) conflict detection.
- Multi-repository hosting in v1 (the schema is repo-keyed so it can be added; Appendix C).
- Code review UI, code search, CI scheduling.
- Windows clients.

### 2.3 Deferred (designed-for, not built in v1)

Regional cache tier, virtualized (FUSE/NFS) working copies, filesystem-watcher `status`, daemon
snapshot bootstrap, chunked large-blob storage, rename/copy detection, path-name redaction,
garbage collection of unreferenced objects, multi-repo, two-way git sync, search index, land
policies (review/CI gates). Details: Appendix A.

## 3. System Overview

### 3.1 Main Components

1. `Object Store`
   - S3-compatible object storage holding zstd-compressed object bytes.
   - Holds all blobs, and trees/metadata objects larger than the inline threshold.

2. `Metadata Store`
   - CockroachDB cluster (SERIALIZABLE isolation).
   - Holds object index, changesets, refs, ref log, landed-change index, derived data, path index,
     redactions, and counters.

3. `scion-server` (authoritative tier; stateless apart from caches; horizontally scalable)
   - `Object Service`: serves and accepts objects; enforces integrity and redaction.
   - `Write Service`: validates and creates changesets; builds trees.
   - `Merge Engine`: deterministic rebase and three-way merge.
   - `Ref Service`: ref resolution, CAS updates, land loop, ref log.
   - `Derivation Engine`: eager and lazy derived data.
   - `Query Service`: log, blame, diff, read-file, ancestry, merge-base.
   - `Auth Service`: GitHub and service-principal login; token issuance.
   - `Policy Engine`: loads `acl.toml`; evaluates path and ref permissions.
   - `Redaction Service`: redaction lifecycle, propagation, purge.

4. `scion-daemon` (per-node caching tier; OPTIONAL per node)
   - Listens on a local Unix socket.
   - Serves read RPCs from a bounded, verified, content-addressed cache; forwards everything else.
   - Shared by all jobs and users on the node.

5. `scion` client (CLI + library)
   - Manages working copies, sparse profiles, the stat cache, and local operation state.
   - Talks to a daemon socket or directly to a server; the protocol is identical.

6. `scion-import`
   - One-way git-to-Scion history importer (Section 14).

### 3.2 Abstraction Levels

1. `Object Layer`: canonical encoding, identifiers, blobs, trees, metadata, changesets.
2. `Storage Layer`: metadata schema, object storage layout, write ordering.
3. `History Layer`: changeset creation, rebase/merge, refs, land.
4. `Derived Layer`: commit graph, history nodes, blame, path index.
5. `Service Layer`: RPC contracts, error model, authentication, authorization.
6. `Cache Layer`: daemon cache, coalescing, epoch-driven invalidation.
7. `Working Copy Layer`: checkout, sparse, status, commit, conflict resolution.
8. `Policy Layer`: `acl.toml`, service principals, redaction authority.

Lower layers MUST NOT depend on higher layers. In particular, the object and storage layers MUST
NOT depend on policy (authorization is applied in the service layer).

### 3.3 Topology and Request Flow

```text
 client ──unix socket──▶ scion-daemon ──TLS gRPC──▶ scion-server ──▶ CockroachDB
   │                        (cache)                     │       └──▶ Object storage
   └────────────TLS gRPC (no daemon)───────────────────┘
```

- Read RPCs are answered by the nearest tier holding the object; misses go one hop upstream.
- Write, ref, query, auth, and admin RPCs are forwarded by the daemon verbatim.
- A tier MUST talk only to the tier directly above it. The topology is invisible to clients.
- A future regional cache tier inserts between daemon and server without protocol changes.

### 3.4 Reference Stack

- Implementation language: Rust for client, daemon, and server.
- RPC: gRPC with protobuf message envelopes (protobuf is used for transport only; object identity
  uses the canonical encoding in Section 4.1, never protobuf serialization).
- Hash: BLAKE3-256. Compression: zstd.
- Metadata: CockroachDB. Objects: S3-compatible storage (MinIO in tests).

### 3.5 External Dependencies

- CockroachDB v23.2 or later.
- S3-compatible object storage with strong read-after-write consistency.
- GitHub OAuth App (device flow) and the GitHub REST API (`GET /user`).
- Unix host for clients and daemons.

## 4. Core Domain Model

### 4.1 Canonical Encoding

All structured objects use one canonical binary encoding. Identity is defined over these bytes.

Primitives:

- `u8`: one byte.
- `uvarint`: unsigned LEB128, at most 10 bytes, MUST be minimally encoded.
- `svarint`: zigzag-mapped `uvarint` for signed 64-bit values.
- `bytes`: `uvarint` length followed by that many octets.
- `list<T>`: `uvarint` count followed by `T` repeated.
- `option<T>`: `u8` 0 (absent) or 1 (present) followed by `T` when present.
- `ObjectId`: 34 raw bytes (Section 4.2).
- `Extensions`: `list<(tag: uvarint, value: bytes)>` with strictly ascending tags; always the final
  field of a structured object.

Decoders MUST reject: non-minimal varints, lengths exceeding remaining input, trailing bytes, lists
that violate their ordering or uniqueness rules, unknown `format_version` values, and unknown
extension tags that are odd (critical). Unknown even (ignorable) extension tags MUST be preserved
byte-for-byte and otherwise ignored. Encoders MUST produce exactly one byte string per logical
value.

### 4.2 Object Identifiers

`ObjectId = algo (u8) || kind (u8) || digest (32 bytes)`

- `algo`: `0x01` = BLAKE3-256. Other values reserved.
- `kind`: `0x01` Blob, `0x02` Tree, `0x03` Changeset, `0x04` Meta. `0x10`-`0x7F` reserved for
  derived objects (Section 9). Other values reserved.
- `digest`:
  - Blob: BLAKE3 of the raw (uncompressed) content.
  - All other kinds: BLAKE3 of the canonical encoding.

Text form: kind letter (`b`, `t`, `c`, `m`; derived kinds `x`), algo as one decimal digit, then 64
lowercase hex digits. Example: `c1` + 64 hex. Clients MAY display and accept unique prefixes of at
least 8 hex digits for changesets.

Because `kind` is part of the identifier, two objects of different kinds never collide even when
their digests are equal.

### 4.3 Entities

#### 4.3.1 Blob

- Raw file bytes (regular file, executable file, or symlink target).
- Stored zstd-compressed; compression level and framing are not part of identity.
- Size limit: `limits.max_blob_bytes` (default 104857600). Hard v1 ceiling: 4 GiB.

#### 4.3.2 Tree

```text
Tree {
  format_version: uvarint = 1
  entries: list<TreeEntry>     // strictly ascending by name, bytewise unsigned compare
  extensions: Extensions
}
TreeEntry {
  name: bytes
  mode: u8                     // 1 File, 2 Executable, 3 Symlink, 4 Tree
  id: ObjectId                 // kind Blob for modes 1-3, kind Tree for mode 4
  size: uvarint                // blob length for modes 1-3; 0 for mode 4
}
```

- Modes correspond to git modes 100644, 100755, 120000, and 040000.
- A non-root tree MUST have at least one entry (no empty directories, matching git).
- The root tree MAY be empty.

#### 4.3.3 Meta

```text
Meta {
  format_version: uvarint = 1
  author: bytes                // conventionally "Name <email>"
  author_time: Timestamp
  committer: option<bytes>     // populated by import; absent for native changesets
  committer_time: option<Timestamp>
  creator: bytes               // principal id that created the changeset (server-set)
  message: bytes
  extensions: Extensions       // tag 2 (ignorable): git commit id from import
}
Timestamp { seconds: svarint, tz_offset_minutes: svarint }
```

Meta is a separate object so that it can be redacted independently (Section 13) without changing
any changeset identifier.

#### 4.3.4 Changeset

```text
Changeset {
  format_version: uvarint = 1
  parents: list<ObjectId>      // kind Changeset; ordered; unique; 0..=limits.max_parents
  root_tree: ObjectId          // kind Tree
  meta: ObjectId               // kind Meta
  change_id: 16 bytes
  delta: list<DeltaEntry>      // strictly ascending by path, bytewise
  extensions: Extensions
}
DeltaEntry {
  path: bytes
  op: u8                       // 1 Put, 2 Delete
  // present only when op = Put:
  mode: u8                     // 1..=3
  id: ObjectId                 // kind Blob
  size: uvarint
}
```

Let `T1` be the root tree of `parents[0]`, or the empty tree when `parents` is empty. A changeset
is valid only if:

- `delta` equals `FileDiff(T1, root_tree)` exactly: the set of file-level paths whose
  `(mode, id)` differs, with `Put` for present-in-result and `Delete` for absent-in-result.
  Consequently the delta is uniquely determined by the two trees, is minimal, and contains no
  no-op entries.
- Exactly one changeset per repository has empty `parents`: the root changeset with an empty
  `root_tree`. (Import may create others; see Section 14.)

`delta` is relative to the first parent only, including for merge changesets.

#### 4.3.5 ChangeId

- 16 random bytes generated by the client when a logical change is first committed.
- Preserved by amend and rebase; identifies "the same change" across revisions for review tools and
  for land idempotency.
- Not unique per changeset. Unique per ref among landed changes (Section 8.4).
- Text form: 32 lowercase hex digits.

#### 4.3.6 Ref

- `(name, target: ChangesetId, version: u64)`.
- Name grammar: `^[a-z0-9][a-z0-9._/-]{0,199}$`, no `..`, no `//`, no trailing `/`.
- v1 refs: `main` (REQUIRED), `release/*`, `green/*` (last-known-good, written by CI). Feature work
  is unnamed changesets addressed by id.
- `version` increments by exactly 1 on every update and starts at 1.

#### 4.3.7 RefLogEntry

`(ref, version, old_target, new_target, principal, at, landed: list<(change_id, source, result)>)`

The ref log is the system's write-ahead log of `main`. `<ref>@<version>` addresses a historical
target.

#### 4.3.8 Principal

- `github:<login>` (humans, via GitHub login), with stable numeric id carried in the token.
- `service:<name>` (agents, CI, importer), via configured secrets.
- `group:<name>` (policy only; never authenticates).

#### 4.3.9 Redaction

`(object_id, state: Quarantined | Purged, reason, actor, created_at, purge_after, epoch)`

#### 4.3.10 Working Copy State

`(root_path, endpoint, base: ChangesetId, tracking_ref, sparse: list<path>, inaccessible:
list<path>, pending_operation: option<Operation>, index: StatCache)`

### 4.4 Path Rules

- A name is 1-255 bytes, contains no `0x00` and no `/`, is not `.` or `..`, and is not `.scion`
  under ASCII case-insensitive comparison.
- Names are arbitrary bytes otherwise (no Unicode normalization; matches git).
- A path is names joined by `/`, at most 4096 bytes, with no leading or trailing `/`. The root
  path is the empty string.
- Paths compare bytewise. Case-only differences are distinct paths; clients on case-insensitive
  filesystems MUST detect and report collisions during checkout rather than silently overwrite.

### 4.5 Global Invariants

1. Every stored object's bytes hash to its identifier.
2. A changeset row exists only if its root tree, meta, all parents, all delta blobs, and all eager
   derived values exist.
3. Nothing is ever mutated in place except refs, counters, policy/redaction state, and lazy derived
   data write-back (which is idempotent and deterministic).
4. No identifier ever changes, including under redaction.

## 5. Storage Specification

### 5.1 Object Storage Layout

- Key: `<prefix>/r/<repo_id>/<kind_letter>/<hex[0:2]>/<hex>` holding a zstd frame of the content.
- Quarantine key: `<prefix>/q/<repo_id>/<kind_letter>/<hex[0:2]>/<hex>`.
- Trees and Meta whose encoding is at most `objects.inline_threshold_bytes` (default 65536) are
  stored inline in the metadata store instead.

### 5.2 Metadata Schema

All tables are keyed by `repo_id` first (single row in `repos` for v1).

| Table | Primary key | Columns |
|---|---|---|
| `repos` | `repo_id` | `name`, `created_at` |
| `objects` | `(repo_id, object_id)` | `kind`, `size`, `stored_size`, `inline_data?`, `state` (`live`/`quarantined`/`purged`), `created_at` |
| `object_uploads` | `(repo_id, object_id, principal)` | `at` |
| `object_paths` | `(repo_id, object_id, path)` | `first_changeset`, `at` |
| `changesets` | `(repo_id, changeset_id)` | `encoded`, `generation`, `skip` (list of ids), `change_id`, `created_by`, `created_at` |
| `changeset_children` | `(repo_id, parent_id, child_id)` | |
| `refs` | `(repo_id, name)` | `target`, `version`, `updated_at` |
| `ref_log` | `(repo_id, name, version)` | `old_target`, `new_target`, `principal`, `at`, `landed` |
| `landed` | `(repo_id, ref, change_id)` | `version`, `source_changeset`, `result_changeset` |
| `derived` | `(repo_id, type, type_version, key)` | `value?`, `storage_key?`, `at` |
| `derive_leases` | `(repo_id, type, type_version, key)` | `holder`, `expires_at` |
| `redactions` | `(repo_id, object_id)` | `state`, `reason`, `actor`, `created_at`, `purge_after`, `epoch` |
| `counters` | `(repo_id, name)` | `value` (`redaction_epoch`) |

Implementations MAY add secondary indexes. Implementations MUST NOT add columns that participate
in object identity.

### 5.3 Write Ordering

1. Object upload: write bytes to object storage (or prepare inline data), then insert the `objects`
   row with `ON CONFLICT DO NOTHING`. A `live` row implies durable bytes.
2. Changeset creation: all referenced objects exist and are `live` (or carried over unchanged from
   the first parent; Section 13.6) -> write new trees -> compute eager derived values -> one
   transaction inserting the `changesets` row, `changeset_children`, `object_paths`, and eager
   `derived` rows, all with `ON CONFLICT DO NOTHING`.
3. Ref update: one transaction performing the version-checked update, the `ref_log` insert, and
   the `landed` inserts.

Bytes in object storage without an `objects` row are orphans. Orphans are harmless and are
reclaimed by deferred GC (Appendix A).

### 5.4 Consistency Rules

- Ref reads at the server MUST be strongly consistent.
- Reads of immutable rows (objects, changesets, derived values) MAY use stale/follower reads; a
  miss under a stale read MUST be retried with a consistent read before reporting `NOT_FOUND`.
- Redaction state reads follow Section 13.3 propagation bounds, not stale-read rules.

## 6. Configuration Specification

All configuration files are TOML. Unknown keys MUST be rejected at startup with a typed error
naming the key. String values of the form `$NAME` resolve from the environment; unresolved
variables are startup errors.

### 6.1 Server Config (`scion-server.toml`)

```toml
[server]
listen = "0.0.0.0:7443"
tls_cert = "/etc/scion/tls.crt"
tls_key = "/etc/scion/tls.key"
repo = "monorepo"

[metadata]
url = "$SCION_DB_URL"
max_connections = 64

[objects]
endpoint = "https://s3.example.internal"
bucket = "scion"
prefix = "prod"
inline_threshold_bytes = 65536
zstd_level = 3

[auth]
github_client_id = "$SCION_GITHUB_CLIENT_ID"
github_api_url = "https://api.github.com"
token_ttl = "12h"
signing_keys = ["/etc/scion/keys/2026-10.ed25519"]   # first key signs; all keys verify

[[auth.service_principals]]
name = "ci"
secret_sha256 = "$SCION_CI_SECRET_SHA256"

[policy]
path = "/etc/scion/acl.toml"
reload = true

[limits]
max_blob_bytes = 104857600
max_delta_entries = 100000
max_parents = 16
max_message_bytes = 1048576
merge_max_text_bytes = 10485760

[land]
max_attempts = 32
backoff_base_ms = 5
backoff_max_ms = 500

[derive]
lease_ttl_ms = 30000
lazy_workers = 16

[redaction]
propagation_bound_ms = 1000
default_quarantine = "7d"

[observability]
log_format = "json"
metrics_listen = "127.0.0.1:9464"
```

### 6.2 Access Policy (`acl.toml`)

```toml
version = 1

[groups]
eng = ["github:alice", "github:bob", "group:agents"]
agents = ["service:agent-pool", "service:ci"]
security = ["github:carol"]
admins = ["github:alice"]

[[paths]]
prefix = "/"
read = ["group:eng"]
write = ["group:eng"]

[[paths]]
prefix = "/secrets"
read = ["group:security"]
write = ["group:security"]

[refs."main"]
update = ["group:eng"]
force = []

[refs."release/*"]
update = ["group:admins"]
force = ["group:admins"]

[refs."green/*"]
update = ["service:ci"]
force = ["service:ci"]

[admin]
redact = ["group:admins"]
purge = ["group:admins"]
refs = ["group:admins"]          # create/delete refs
```

Semantics:

- `prefix` is a component-wise path prefix written with a leading `/`. `/secrets` matches
  `secrets` and `secrets/x` but not `secrets2`.
- A `/` rule is REQUIRED.
- For a path, the rule with the longest matching prefix governs, independently for `read` and
  `write`. Rules do not inherit or merge.
- Principals may be given as `github:<login>` (case-insensitive) or `github-id:<number>`.
- Groups may nest; cycles are validation errors.
- Ref patterns support a single trailing `*` segment. Exact names win over patterns; longest
  pattern wins among patterns.
- Anything not granted is denied.

Reload semantics:

- When `policy.reload = true`, the server watches the file. A valid new policy is swapped in
  atomically and increments the server's in-memory `policy_epoch` (a hash of the normalized
  policy, so all servers loading the same file agree).
- An invalid file MUST NOT replace the last known good policy; the server emits an operator-visible
  error and continues.
- Startup with an invalid or missing policy file MUST fail.

### 6.3 Daemon Config (`scion-daemon.toml`)

```toml
socket = "/run/scion/daemon.sock"
upstream = "https://scion.example.internal:7443"
cache_dir = "/var/cache/scion"
max_cache_bytes = 53687091200
low_watermark_ratio = 0.8
verify_on_read = true
ref_cache_ttl_ms = 1000
access_cache_ttl_ms = 60000
redaction_poll_interval_ms = 10000
redaction_max_staleness_ms = 600000
upstream_max_concurrency = 256
```

### 6.4 Client Config

- User config: `~/.config/scion/config.toml` (`endpoint = "unix:///run/scion/daemon.sock"` or an
  `https://` URL).
- Credentials: `~/.config/scion/credentials`, mode `0600`, holding the Scion token and OPTIONALLY
  the GitHub OAuth token for silent re-exchange.
- Environment overrides: `SCION_ENDPOINT`, `SCION_TOKEN`.
- Working copy state: `<root>/.scion/` (Section 11.1).

### 6.5 Defaults Cheat Sheet

| Key | Default |
|---|---|
| `objects.inline_threshold_bytes` | 65536 |
| `objects.zstd_level` | 3 |
| `auth.token_ttl` | 12h |
| `limits.max_blob_bytes` | 100 MiB |
| `limits.max_delta_entries` | 100000 |
| `limits.max_parents` | 16 |
| `limits.merge_max_text_bytes` | 10 MiB |
| `land.max_attempts` | 32 |
| `redaction.propagation_bound_ms` | 1000 |
| `redaction.default_quarantine` | 7d |
| `daemon.max_cache_bytes` | 50 GiB |
| `daemon.ref_cache_ttl_ms` | 1000 |
| `daemon.access_cache_ttl_ms` | 60000 |
| `daemon.redaction_poll_interval_ms` | 10000 |
| `daemon.redaction_max_staleness_ms` | 600000 |

## 7. RPC Protocol

### 7.1 Transport

- gRPC over TLS between daemon/client and server; gRPC over the Unix socket between client and
  daemon. Package `scion.v1`.
- Authentication: `authorization: Bearer <scion token>` request metadata on every RPC except
  `GetServerInfo` and `Login`.
- Every response MUST carry trailers `scion-policy-epoch`, `scion-redaction-epoch`, and
  `scion-tier` (`server` or `daemon`).
- Breaking changes require a new package (`scion.v2`) served side by side. Non-breaking additions
  use new fields and new RPCs only.
- Batch RPCs return per-item results; one denied or missing item MUST NOT fail the batch.

### 7.2 Error Model

Errors use a gRPC status plus a `scion.v1.ErrorDetail { code, retryable, message, payload }`.

| `code` | gRPC status | Retryable | Meaning |
|---|---|---|---|
| `NOT_FOUND` | NOT_FOUND | no | Object, changeset, path, or ref absent |
| `PERMISSION_DENIED` | PERMISSION_DENIED | no | Policy denies |
| `UNAUTHENTICATED` | UNAUTHENTICATED | no | Missing, invalid, or expired token |
| `INVALID_ARGUMENT` | INVALID_ARGUMENT | no | Validation failure; `payload.subcode` names the rule |
| `TOO_LARGE` | INVALID_ARGUMENT | no | A configured limit exceeded |
| `INTEGRITY` | DATA_LOSS | yes (other source) | Bytes do not match identifier |
| `REDACTED` | FAILED_PRECONDITION | no | Object is redacted |
| `MERGE_CONFLICT` | ABORTED | no | `payload` lists conflicts (Section 8.3) |
| `AMBIGUOUS_MERGE_BASE` | FAILED_PRECONDITION | no | Multiple best common ancestors |
| `REF_VERSION_MISMATCH` | ABORTED | yes | CAS lost; `payload` has current version and target |
| `NOT_FAST_FORWARD` | FAILED_PRECONDITION | no | Update would drop history |
| `ALREADY_LANDED` | ALREADY_EXISTS | no | ChangeId landed from a different source changeset |
| `EMPTY_CHANGE` | FAILED_PRECONDITION | no | Rebase result has an empty delta |
| `RESOURCE_EXHAUSTED` | RESOURCE_EXHAUSTED | yes | Rate limited or shedding load |
| `UNAVAILABLE` | UNAVAILABLE | yes | Dependency down or timeout |

### 7.3 Object RPCs (served by every tier)

- `GetServerInfo() -> {protocol_versions, repo, policy_epoch, redaction_epoch, token_public_keys,
  limits}`
- `GetObjects(ids, options{offset?, length?}) -> stream {id, status, encoding (raw|zstd), data,
  total_size, access_class (root|restricted), final}`
  - The server MAY stream stored zstd frames unmodified. The receiver MUST verify the BLAKE3 digest
    of decompressed content before use or caching. Range reads return raw bytes.
  - `status` per item is one of `ok`, `not_found`, `denied`, `redacted`.
- `HasObjects(ids) -> list<bool>` (semantics in Section 12.5)
- `ResolveRef(name, at_version?) -> {target, version}`

### 7.4 Query RPCs (server; forwarded by daemon)

- `GetChangesets(ids) -> per-item {encoded | filtered_view, status}`
- `ReadPath(changeset, path) -> {entry, content stream?}`: one call for API-only jobs.
- `ListTree(changeset, path) -> entries`
- `Log(from, path?, first_parent_only?, limit, page_token) -> {changesets, next_page_token}`
- `Blame(changeset, path) -> list<{start_line, line_count, origin_changeset, origin_start_line}>`
- `Diff(a, b, paths?, include_content_hunks?) -> stream {path, old_entry, new_entry, hunks?}`
- `IsAncestor(a, b) -> bool`
- `MergeBase(a, b) -> list<ChangesetId>`
- `Resolve(revision) -> ChangesetId` (revision grammar: Section 11.8)

### 7.5 Write RPCs (server)

- `UploadObjects(stream {id, kind, data}) -> per-item {status}`: verifies digests; rejects redacted
  ids; records `object_uploads`.
- `CreateChangeset({parents, delta, meta_fields, change_id, expected_root_tree?}) ->
  {changeset_id, root_tree, meta_id}`: server builds trees and the Meta object (setting
  `creator`).
- `Rebase(changeset, onto) -> {changeset_id} | MERGE_CONFLICT{clean_entries, conflicts}`
- `Merge(a, b, meta_fields, change_id) -> {changeset_id} | MERGE_CONFLICT{...}`
- `Land(ref, head, options{allow_empty=false}) -> {target, version, landed}`

### 7.6 Ref RPCs (server)

- `UpdateRef(name, expected_version, new_target, allow_non_fast_forward=false) -> {version}`
- `CreateRef(name, target)` and `DeleteRef(name, expected_version)`: require `admin.refs`.
- `GetRefLog(name, from_version, limit)`

### 7.7 Auth and Admin RPCs (server)

- `Login({github_access_token} | {service_name, service_secret}) -> {token, principal, expires_at}`
- `WhoAmI() -> {principal, groups, expires_at}`
- `CheckAccess(subject_token, object_ids) -> list<bool>` (daemon use; Section 10.4)
- `Redact(object_ids, reason, purge_after?) -> {epoch, effective_by}`
- `Unredact(object_ids)`: only from `Quarantined`
- `PurgeNow(object_ids)`
- `ListRedactions(since_epoch) -> {entries, current_epoch}`

### 7.8 Tier Conformance Rule

A daemon MUST implement Section 7.3 RPCs with response semantics identical to the server (byte-
identical object payloads after decompression, identical per-item statuses for the same principal,
policy epoch, and redaction epoch). All other RPCs MUST be forwarded without modification.

## 8. Write Path: Changesets, Rebase, Merge, and Land

### 8.1 CreateChangeset

Validation, in order (first failure returns `INVALID_ARGUMENT` with subcode):

1. `parents` exist, are unique, count within `limits.max_parents`; empty only if the repository has
   no root changeset yet (or by importer, Section 14).
2. `delta` sorted, unique, within `limits.max_delta_entries`, paths valid (Section 4.4).
3. Each `Put` blob exists and is `live`; its `size` matches the stored size. A blob the caller
   neither uploaded nor can read (12.5) is treated as missing.
4. No `Put` references a redacted object (`REDACTED`).
5. Each `Delete` path is a file in `T1`; each `Put` differs from `T1` at that path.
6. File/directory consistency: a `Put` whose ancestor is a file in `T1` requires a `Delete` of that
   file in the same delta; a `Put` at a path that is a directory in `T1` requires `Delete`s of every
   file below it.
7. The principal can write every delta path (Section 12.4).
8. Meta fields within limits.

Then: build the result tree (Section 18.2); if `expected_root_tree` is given and differs, fail
`INVALID_ARGUMENT/root_tree_mismatch`; build Meta; compute the identifier; persist per Section 5.3.

Creating an identical changeset twice MUST return the same identifier and MUST NOT error.

### 8.2 Rebase

`Rebase(C, X)` with `P = C.parents[0]`, requiring `len(C.parents) == 1` in v1:

- Base `B = tree(P)`, ours `O = tree(X)`, theirs `T = tree(C)`.
- Only paths in `C.delta` can differ between the result and `O`. The merge examines exactly those
  paths plus their ancestor directories in `O` (O(|delta| x depth)).
- If `X == P`, the result is `C` itself (fast path).
- The result changeset has `parents = [X]`, the same `meta` and `change_id` as `C`, the merged
  tree, and the recomputed delta against `O`.
- No wall-clock value enters the result. Rebase is a pure function of `(C, X, merge engine
  version)`, so identical requests produce identical identifiers.
- If the recomputed delta is empty, return `EMPTY_CHANGE` (the change is already present in `X`).

### 8.3 Three-Way Merge Rules

Per path `p`, with `b = B[p]`, `o = O[p]`, `t = T[p]` (each is absent or `(mode, id)`):

| Condition | Result |
|---|---|
| `o == b` | take `t` |
| `t == b` | take `o` |
| `o == t` | take `o` |
| `b` absent, `o` and `t` present and differ | conflict `add_add` |
| one of `o`, `t` absent, the other differs from `b` | conflict `modify_delete` |
| any side is a symlink and sides differ | conflict `symlink` |
| either side binary or larger than `limits.merge_max_text_bytes` | conflict `binary` |
| any needed side's content is redacted | conflict `redacted` |
| otherwise | content merge of `b`, `o`, `t`; mode merged by the same first three rows |

Additional structural conflicts:

- `t` puts a file at `p` where `O` has a directory at `p`, or a file at an ancestor of `p`:
  conflict `file_directory`.
- `t` deletes `p` while `O` has a directory at `p`: conflict `file_directory`.

Binary detection: content contains `0x00` within its first 8000 bytes (git's heuristic).

Content merge (`merge_engine = "line-diff3-v1"`):

- Split into lines on `\n`, retaining terminators. A final unterminated line is a line.
- Compute line diffs `b->o` and `b->t` with the Myers algorithm using the reference
  implementation's deterministic tie-breaking (the reference implementation is normative for
  output).
- Hunks whose base ranges overlap, or are adjacent with zero unchanged lines between them, conflict
  unless their replacement content is identical (then apply once).
- Non-conflicting hunks from both sides apply. The result is a new blob.
- Conflict markers are NEVER written into server-side objects. Conflicts are returned as data:
  `{path, kind, base?, ours?, theirs?, hunks?}` plus `clean_entries` for all non-conflicting paths.

`Merge(a, b)` (two-parent merge changesets) uses the same rules over `FileDiff` of the unique merge
base to each side, pruning identical subtree ids. If more than one best common ancestor exists,
return `AMBIGUOUS_MERGE_BASE`.

### 8.4 Land

`Land(ref, head)` lands a linear stack onto `ref` without locks.

Stack: the chain of first parents from `head` back to, but excluding, the first changeset that is
an ancestor of the current ref target. v1 requires every stack member to have exactly one parent.

Per-member landed check, using the `landed` index for `(ref, change_id)`:

- landed with `source == member`: skip (already on the ref).
- landed with a different source: fail `ALREADY_LANDED` (an amended revision of a landed change).
- not landed: rebase.

Land states:

1. `Resolving`: read target `T` and version `v` (strong read).
2. `Checking`: classify stack members as above. If every member is skipped, return success with the
   recorded result (idempotent retry).
3. `Rebasing`: rebase remaining members in order onto `T`, chaining results.
4. `Committing`: transaction: update `refs` where `version = v` to the final result at `v + 1`;
   insert `ref_log`; insert `landed` rows for each rebased member.
5. `Succeeded`: return `{target, version, landed}`.
6. `Retrying`: on `REF_VERSION_MISMATCH` or a `landed` unique violation, back off (full jitter,
   `land.backoff_base_ms` to `land.backoff_max_ms`) and return to `Resolving`.
7. `Failed`: on `MERGE_CONFLICT`, `EMPTY_CHANGE` (unless `allow_empty`), permission failures, or
   after `land.max_attempts`.

Authorization is checked in `Resolving` on every attempt against the current policy: `update` on
the ref and `write` on every delta path of every rebased member.

Landing is lock-free: no server holds a lock or lease across attempts; progress depends only on the
single-row CAS.

### 8.5 UpdateRef

- Version-checked update; `REF_VERSION_MISMATCH` returns current `(version, target)`.
- Requires `update` permission. A non-fast-forward update (new target not a descendant of the old)
  requires `allow_non_fast_forward` and `force` permission, else `NOT_FAST_FORWARD`.
- Writes a `ref_log` entry in the same transaction.

## 9. Derived Data Framework

### 9.1 Contract

```text
DerivedType {
  name: string                  // stable, e.g. "commit_graph"
  version: u32                  // bump to re-derive; old versions remain readable until GC
  mode: Eager | Lazy
  key(changeset) -> bytes       // or a type-specific key (e.g. a history node id)
  derive(changeset, parent_values, ctx) -> Value
  content_bearing: bool         // stores file content (affects redaction; Section 13.5)
}
```

Requirements:

- `derive` MUST be deterministic: identical inputs produce byte-identical values.
- Values are written with `ON CONFLICT DO NOTHING`; concurrent derivations are harmless.
- `Eager` types are computed before the changeset row is inserted (Section 5.3).
- `Lazy` types are computed on first query. The engine finds the nearest ancestors with stored
  values and derives forward iteratively (never by unbounded recursion), writing back each value.
- Leases (`derive_leases`) only deduplicate work; correctness MUST NOT depend on them.
- Content-bearing types MUST key every content-derived record by the source object id so redaction
  can find and delete it.
- Adding a type or bumping a version MUST NOT require any client change.

### 9.2 Required Types (v1)

1. `commit_graph` (Eager)
   - `generation = 1 + max(parent generations)` (root = 1).
   - `skip[k]` = first-parent ancestor at distance `2^k`, for `k` while it exists.
   - Stored on the `changesets` row.
   - Used by `IsAncestor`, `MergeBase`, `Log`, and land stack detection.

2. `history` (Eager)
   - A history node for every path in the tree:
     `HistoryNode { kind: File|Dir, content: ObjectId, changeset, parents: list<HistoryNodeId>,
     children (Dir): list<(name, HistoryNodeId)>, extensions }`, identified as derived kind `0x10`.
   - Node reuse: if a path's content in `C` equals its content in `parents[0]`, reuse that parent's
     node. Otherwise create a node whose `parents` are the nodes for that path in each parent where
     it exists (deduplicated, parent order).
   - This follows only the first parent when a merge keeps the first parent's version, matching
     git's default history simplification.
   - Cost per changeset: O(|delta| x depth) new nodes.
   - Per-changeset value: the root history node id.

3. `blame` (Lazy; not content-bearing: stores line ranges and changeset ids only)
   - Keyed by file history node id.
   - For node `N` with content `c`: diff against the first parent node's content, attribute matching
     lines from that parent's blame; repeat with subsequent parents for remaining lines; remaining
     lines belong to `N.changeset`.
   - Redacted content is treated as empty for derivation.

4. `path_index` (Eager)
   - Rows in `object_paths` for every blob introduced by a `Put` and every new tree at its directory
     path. Used for authorization (Section 12.5) and redaction tooling.

5. `git_mapping` (written by importer)
   - `git:<sha> -> changeset` and `changeset -> git:<sha>`.

## 10. Fetch Protocol and Daemon Cache

### 10.1 Fetch Walk

The client never describes what it has. It requests what it wants:

1. `ResolveRef` or `Resolve` -> changeset; `GetChangesets` -> `root_tree`.
2. Breadth-first by depth: request missing trees within the sparse scope in batches; skip subtrees
   outside the scope; record `denied` subtrees as inaccessible.
3. Request missing blobs for files in scope, in batches, streaming.
4. Each response item is verified before it is written to any cache or the working copy.

Resumption: a broken stream is resumed by re-requesting incomplete objects with `offset`.

### 10.2 Daemon Object Cache

- Layout: `<cache_dir>/objects/<kind_letter>/<hex[0:2]>/<hex>` (as received, compressed).
- Insert: write to a temp file in the same directory, verify the digest, `fsync`, rename.
- Startup: delete temp files; MAY lazily verify on read when `verify_on_read = true`.
- A digest mismatch on read deletes the entry and re-fetches.
- Eviction: approximate LRU by access time when usage exceeds `max_cache_bytes`, down to
  `low_watermark_ratio`.
- Request coalescing: concurrent requests for the same missing object produce exactly one upstream
  request.
- Upstream concurrency is capped at `upstream_max_concurrency`; excess requests queue, and queue
  overflow returns `RESOURCE_EXHAUSTED`.

### 10.3 Ref Caching

- The daemon MAY cache `ResolveRef` results for at most `ref_cache_ttl_ms`.
- For a given ref, the daemon MUST NOT return a lower `version` than it has previously returned
  (monotonic reads).

### 10.4 Daemon Authorization

- The daemon verifies tokens locally with keys from `GetServerInfo` (signature, expiry).
- Objects with `access_class = root` (readable under the `/` rule) are served to principals the
  daemon has confirmed can read `/`, via `CheckAccess` cached for `access_cache_ttl_ms`.
- Objects with `access_class = restricted` require a positive `CheckAccess(principal, object)`
  decision cached for at most `access_cache_ttl_ms`.
- All access-class and decision caches are dropped when a response carries a different
  `scion-policy-epoch`.
- The daemon MUST NOT serve an object to a principal on the basis of another principal's decision.

### 10.5 Tier Isolation

- The daemon applies backpressure and never retries upstream more than three times per request
  (exponential backoff with jitter).
- The server applies per-principal rate limits (`RESOURCE_EXHAUSTED`) and sheds load before its
  dependencies saturate.

## 11. Client and Working Copy

### 11.1 Layout

```text
<root>/.scion/
  config.toml        # endpoint override, tracking ref
  state              # base, sparse profile, inaccessible paths, pending operation (canonical TOML)
  index              # stat cache
  conflicts/         # base/ours/theirs copies during conflict resolution
  lock               # advisory lock; one mutating command at a time
```

### 11.2 Sparse Profile

- An ordered set of path prefixes (directories or files). `""` means the whole tree.
- A file is in scope if a profile entry equals it or is an ancestor directory of it.
- Directories on the way to a scoped prefix are created, but their direct files are not
  materialized unless in scope.
- `scion sparse add|remove` materializes or removes exactly the affected files. Removing scope that
  contains dirty files fails unless `--force`.

### 11.3 Checkout and Goto

- `checkout`: fetch walk (Section 10.1), then write files atomically (temp + rename in the target
  directory), setting the executable bit and creating symlinks as recorded.
- Writes MUST NOT follow pre-existing symlinks in parent directories (use `openat` with
  `O_NOFOLLOW` semantics or an equivalent check).
- `goto <rev>`: diff old and new trees within scope, pruning identical subtree ids, and apply only
  the differences. Dirty files the diff would touch abort the command unless `--force`; other dirty
  files are preserved.
- Mutating commands record `pending_operation` before changing files and clear it after. A
  subsequent command that finds a pending operation MUST complete or roll it back before doing
  anything else.

### 11.4 Ignore Rules

`.gitignore` files are honored with git semantics (including nested files and negation). Ignored
files are never committed and never reported as new.

### 11.5 Status

- Walk the in-scope materialized files. An entry is clean if its stat tuple
  `(size, mtime_ns, ctime_ns, inode, mode)` matches the index and it is not racily clean.
- Racily clean: `mtime >= index_write_time`; such files are re-hashed.
- Otherwise hash the file (BLAKE3) and compare with the base tree entry.
- Report `modified`, `added` (untracked, not ignored, in scope), `deleted`, `mode_changed`.
- `status` MUST NOT contact the server when every needed tree is cached locally.

### 11.6 Commit and Amend

- `commit`: compute the delta of the working copy against `base` within scope (all changes; no
  staging area; `--path` restricts), upload missing blobs (`HasObjects` then `UploadObjects`),
  then `CreateChangeset(parents=[base], ...)`. A new `change_id` is generated. `base` becomes the
  new changeset.
- `amend`: `CreateChangeset(parents=base.parents, ...)` with the same `change_id`, reusing the
  original meta unless a new message is given.
- Commits cannot include paths outside scope or inaccessible paths.

### 11.7 Rebase, Land, and Conflict Resolution

- `land [--ref main]`: `Land(ref, base)`. On success, `base` becomes the landed result.
- `rebase --onto <rev>`: server `Rebase` per stack member.
- On `MERGE_CONFLICT`: `goto` the onto changeset, write `clean_entries`, then for each conflict:
  - text: write the file with markers `<<<<<<< ours`, `||||||| base`, `=======`, `>>>>>>> theirs`;
  - other kinds: write the `ours` version (or nothing if absent) and save all sides under
    `.scion/conflicts/`.
- Record `pending_operation = Rebase{original, onto, remaining_stack, conflicts}`.
- `resolve <path>` marks a path resolved. It refuses if conflict markers remain, unless `--force`.
- `commit` while rebasing requires all conflicts resolved and creates the changeset with the
  original's `change_id`, author, and message; the rebase then continues with the remaining stack.
- `rebase --abort` returns to the original changeset.

### 11.8 Commands and Revisions

Core commands: `login`, `whoami`, `checkout`, `sparse`, `status`, `diff`, `commit`, `amend`,
`goto`, `rebase`, `resolve`, `land`, `log`, `blame`, `show`, `cat`, `redact` (admin).

Revision grammar: changeset id or unique prefix (>= 8 hex), ref name, `<ref>@<version>`,
`git:<sha>` (imported), suffixes `~<n>` (n-th first-parent ancestor) and `^<k>` (k-th parent).

Every command supports `--json` with a documented, versioned schema.

## 12. Authentication and Access Control

### 12.1 Tokens

- Server-issued tokens are signed with Ed25519 and contain `sub` (principal), `gid` (GitHub numeric
  id, if any), `iat`, `exp`, and `kid`. The encoding (for example PASETO v4.public) is
  implementation-defined.
- Default lifetime: `auth.token_ttl` (12h). Multiple verification keys allow rotation.

### 12.2 GitHub Login (humans)

1. The client obtains `github_client_id` from `GetServerInfo` and runs the GitHub OAuth device flow
   directly with GitHub.
2. The client calls `Login{github_access_token}`.
3. The server calls `GET /user` and maps `login` and `id` to `github:<login>`.
4. The server returns a Scion token. The GitHub token is not stored server-side.

The client MAY store the GitHub token (credentials file, `0600`) to re-exchange silently on expiry.

### 12.3 Service Principals (agents, CI, importer)

- `Login{service_name, service_secret}`; the server compares `SHA-256(secret)` in constant time
  against `auth.service_principals`.
- Secrets are supplied via environment or secret files and MUST NOT appear in logs.

### 12.4 Path and Ref Authorization

- Path read/write: the longest-prefix rule (Section 6.2).
- Repository read: read on `/`.
- `CreateChangeset`: write on every delta path.
- `Land` / `UpdateRef`: `update` (and `force` for non-fast-forward) on the ref, plus write on every
  delta path of rebased members.
- Admin RPCs: the matching `[admin]` permission.

### 12.5 Object Authorization

Objects are path-independent, so authorization uses `object_paths`:

- An object is readable by principal `P` if some `(object, path)` row has a path `P` can read under
  the current policy. This makes policy changes take effect immediately and retroactively without
  reindexing.
- `access_class = root` if some row's path is governed by the `/` rule, else `restricted`.
- `CreateChangeset` adds an `object_paths` row for every `Put`, so it MUST NOT let a caller
  reference a blob the caller neither uploaded nor can read: such a `Put` fails `missing_blob`,
  exactly like an absent blob, so the error reveals neither existence nor size (8.1 step 3).
- Unreferenced uploaded objects (no `object_paths` rows) are not readable by anyone through
  `GetObjects`.
- `HasObjects` returns `true` only if the object exists and is readable by the caller, or the caller
  uploaded it. Otherwise it returns `false`, even if the object exists, to prevent existence
  probing. A redundant upload of an existing object is accepted and deduplicated.
- Evaluation caches per-object decisions keyed by `(principal, object, policy_epoch)`.

### 12.6 History Filtering

- `GetChangesets` returns a `filtered_view` (delta entries outside the caller's read scope removed,
  `filtered = true`) when any delta path is unreadable. The full encoding is returned only when
  every delta path is readable.
- `ReadPath`, `ListTree`, `Log(path)`, `Blame`, `Diff`: require read on the paths involved; `Diff`
  and `Log` without a path omit unreadable entries.
- Tree listings show restricted subdirectory entries (name and id) in readable parents; fetching
  the subtree is denied (limitation, Appendix A).
- Meta (author, message) is visible to every repository reader unless redacted (limitation,
  Appendix A).

## 13. Redaction of Immutable Data

### 13.1 Model

Scion never rewrites history to delete data. It removes bytes while keeping every identifier:

- Trees reference blobs by id; changesets reference Meta by id. Deleting the bytes of a blob or
  Meta object changes no tree, changeset, or ref.
- Content addressing makes redaction exact and efficient: one redaction removes that content from
  every path and every changeset in which it appears. The cost is O(objects redacted + cached
  copies), independent of history size.
- The redacted id becomes a permanent blocklist entry: the same content cannot be re-introduced
  (Section 13.6).

Redaction is not credential revocation. Leaked secrets MUST still be rotated.

Redactable in v1: Blob and Meta objects. Not redactable in v1: tree entry names, changeset headers
(see Appendix A and Appendix C for the serve-time masking path).

### 13.2 Lifecycle

```text
Live --Redact--> Quarantined --(purge_after elapsed | PurgeNow)--> Purged
                     |
                     +--Unredact--> Live
```

1. `Redact(ids, reason, purge_after = redaction.default_quarantine)` (permission `admin.redact`):
   one transaction inserts `redactions` rows, sets `objects.state = quarantined`, and increments
   `counters.redaction_epoch`. The new epoch is stamped on the rows.
2. Quarantine: the bytes are moved to the quarantine key (copy, then delete the live key) and served
   to no one.
3. `Unredact` (permission `admin.redact`): only from `Quarantined`; restores bytes and state and
   increments the epoch.
4. Purge (permission `admin.purge` for `PurgeNow`; otherwise the scheduled job): delete quarantined
   bytes and any inline data, delete content-bearing derived records keyed by the id, and set
   `state = purged`. Irreversible.

Tooling: `scion redact --path <p> --at <rev> [--all-versions]` resolves ids. `--all-versions` walks
the path's history nodes and lists every distinct blob the path ever held. `scion redact --object
<id>` and `--meta <rev>` are also supported.

### 13.3 Propagation

- Every server refreshes its in-memory redaction set whenever it observes a higher epoch, and at
  least every `redaction.propagation_bound_ms`. No server serves a redacted object later than
  `effective_by = commit_time + propagation_bound_ms`.
- Daemons learn the epoch from response trailers and by polling `ListRedactions(since_epoch)` every
  `redaction_poll_interval_ms`. Before serving any cached object, the daemon checks its local
  redaction set. On learning of a redaction, it deletes cached copies.
- If a daemon cannot refresh for longer than `redaction_max_staleness_ms`, it MUST stop serving from
  cache (fail closed) and forward all reads until a refresh succeeds.

### 13.4 Read Behavior

- `GetObjects` returns per-item status `redacted`. Tree entries still carry the id and size.
- `ReadPath` returns `REDACTED`. `Log` and `show` render redacted Meta as `<redacted>`.
- Integrity verification of a redacted object is skipped. Its absence is not corruption.

### 13.5 Derived Data

- Content-bearing derived records keyed by the id are deleted at purge.
- Non-content-bearing values (blame, history, commit graph) are retained. Future re-derivations
  treat redacted content as empty, so results stay deterministic.

### 13.6 Writes Involving Redacted Objects

- `UploadObjects` and `CreateChangeset` `Put` entries referencing a redacted id fail with
  `REDACTED`.
- Trees that carry a redacted entry over unchanged from the first parent remain valid. Changesets
  that do not touch the path are unaffected.
- Rebase and merge treat redacted content as a `redacted` conflict whenever the content is needed.

### 13.7 Working Copies

- `checkout` and `goto` write a placeholder for redacted files:
  `Scion: contents redacted (object <id>).\n`. The index marks the path `redacted`, and `status`
  treats an unmodified placeholder as clean.
- On its next mutating command, the client replaces clean materialized copies of newly redacted
  blobs with the placeholder. Dirty files are never touched; they are reported.
- Editing a placeholder and committing creates new content normally.

### 13.8 Backups

Implementations MUST document their backup retention. Purged content persists in backups until
they expire, so retention SHOULD NOT exceed 30 days.

## 14. Git Import

`scion-import --git <repo> --ref <git-ref> --target-ref main [--skip-submodules]`

- Runs as a service principal with write on `/` and `admin.refs`.
- Walks commits in topological order (parents first). For each commit:
  - Upload blob contents not yet present.
  - `delta = FileDiff(first parent's git tree, commit tree)` mapped to Scion modes; git mode 100664
    maps to File.
  - Meta: author, author time and zone, committer, committer time and zone, message, extension
    tag 2 = git commit id.
  - `change_id = BLAKE3("scion-import-v1" || git_commit_id)[0:16]`. Re-imports are idempotent.
  - Commits with no parents create parentless changesets (permitted for the importer only).
- Gitlinks (submodules) fail the import unless `--skip-submodules` is given, which omits them and
  records the omission in a Meta extension.
- After the walk: create or update the target ref, and write `git_mapping`.
- Incremental: re-running imports only new commits (mapping lookup) and fast-forwards the ref.

Fidelity requirement: for every imported commit, the Scion tree's file set, modes, and contents
equal the git tree's (excluding skipped gitlinks and empty git trees, which are dropped).

## 15. Logging, Metrics, and Observability

### 15.1 Logs

- Structured (JSON by default), one event per line.
- REQUIRED fields where applicable: `request_id`, `rpc`, `tier`, `principal`, `repo`, `status`,
  `duration_ms`, `changeset_id`, `ref`, `ref_version`, `land_attempt`.
- Secrets, tokens, and file contents MUST NOT be logged. Paths under restricted prefixes MUST NOT
  appear in logs readable by principals lacking access (implementation-defined log access policy).

### 15.2 Metrics (Prometheus)

- `scion_rpc_duration_seconds{rpc,tier,code}` histogram.
- `scion_cache_requests_total{tier,kind,result=hit|miss}` and `scion_cache_bytes{tier}`.
- `scion_land_attempts{result}`, `scion_ref_cas_failures_total{ref}`, `scion_land_duration_seconds`.
- `scion_derive_duration_seconds{type,mode}`, `scion_derive_backlog{type}`.
- `scion_redaction_epoch{tier}`, `scion_redaction_lag_seconds{tier}`, `scion_policy_epoch_info`.
- `scion_object_bytes_served_total{tier,encoding}`, `scion_upstream_requests_total{tier}`.

### 15.3 Health

`/healthz` (process alive) and `/readyz` (dependencies reachable, policy loaded) on the metrics
listener.

Tracing (OpenTelemetry) is RECOMMENDED.

## 16. Failure Model and Recovery

### 16.1 Failure Classes

1. Metadata store unavailable: writes fail `UNAVAILABLE`; reads continue from caches for
   immutable data; ref reads fail at the server (the daemon serves its TTL cache only).
2. Object storage unavailable or slow: reads fail over cache misses; writes fail `UNAVAILABLE`.
3. Server crash mid-upload: orphan bytes (harmless).
4. Server crash mid-`CreateChangeset`: no row, therefore the changeset is not visible; client retry
   yields the same id.
5. Server crash after CAS commit, before response: client retries `Land`; the `landed` index
   returns the recorded result.
6. Daemon crash: temp files deleted at startup; cache entries remain valid.
7. Corrupt object anywhere: detected by digest; the entry is discarded and re-fetched upstream; the
   server reports `INTEGRITY` and alerts if its own copy is corrupt.
8. Client crash mid-operation: `pending_operation` is completed or rolled back on the next command.
9. Invalid policy reload: last good policy retained; error surfaced.
10. GitHub unavailable: existing tokens continue to work; new human logins fail.

### 16.2 Recovery Invariants

- No failure may produce a visible changeset that violates Section 4.5.
- No failure may move a ref to a target that is not the result of a successful, authorized update.
- No failure may cause a daemon or server to serve bytes that do not match their identifier.

## 17. Security and Operational Safety

- TLS is REQUIRED between daemons/clients and servers. mTLS is OPTIONAL.
- The daemon socket permits all local users. Authorization rests on per-request tokens, never on
  socket identity.
- The credentials file is created `0600`; the client refuses to read it if group/world readable.
- Checkout path safety: reject any tree entry violating Section 4.4 before writing. Never write
  outside the working copy root; never follow symlinks while writing.
- The server rejects any object whose decoded form violates Section 4 before persisting.
- Admin actions (redaction, ref create/delete, forced updates) are recorded in an append-only audit
  log with principal and reason.

## 18. Reference Algorithms (Language-Agnostic)

### 18.1 CreateChangeset

```text
function create_changeset(principal, req):
  validate_parents(req.parents)
  validate_delta_shape(req.delta)                    # sorted, unique, limits, path rules
  t1 = root_tree_of(req.parents[0]) if req.parents else EMPTY_TREE
  for e in req.delta:
    if e.op == Put:
      obj = objects.get(e.id) or fail(INVALID_ARGUMENT, "missing_blob")
      if not (uploaded_by(e.id, principal) or can_read_object(principal, e.id)):
        fail(INVALID_ARGUMENT, "missing_blob")       # 12.5: no copy-by-id of unreadable blobs
      if obj.state != live: fail(REDACTED)
      if obj.size != e.size: fail(INVALID_ARGUMENT, "size_mismatch")
    require policy.can_write(principal, e.path)
  root, new_trees = apply_delta(t1, req.delta)       # 18.2; validates minimality and file/dir rules
  if req.expected_root_tree and root != req.expected_root_tree: fail(INVALID_ARGUMENT, "root_tree_mismatch")
  meta = build_meta(req.meta_fields, creator=principal)
  cs = Changeset{parents=req.parents, root_tree=root, meta=id(meta), change_id=req.change_id,
                 delta=req.delta}
  cs_id = id(cs)
  store_inline_or_object(new_trees + [meta])
  eager = {t.name: t.derive(cs, parent_values(t, cs)) for t in eager_types}
  txn:
    insert changesets(cs_id, encode(cs), eager.commit_graph) on conflict do nothing
    insert changeset_children for each parent
    insert object_paths for each Put (blob, path) and each new tree (tree, dir_path)
    insert derived rows for eager values
  return cs_id
```

### 18.2 Apply Delta (tree construction)

```text
function apply_delta(t1, delta):
  # group entries by directory; walk only touched directories
  touched_dirs = ancestors_of_all(delta.paths)       # includes root ""
  for dir in touched_dirs ordered deepest-first:
    entries = load_tree_entries(t1, dir)             # empty if dir absent in t1
    # 1. deletes
    for e in delta where parent(e.path) == dir and e.op == Delete:
      require entries[name(e.path)] is a file else fail("delete_not_file")
      remove entries[name(e.path)]
    # 2. rebuilt child directories (already built, deepest-first)
    for child_dir in direct children of dir in touched_dirs:
      if entries[name(child_dir)] is a file: fail("file_directory")   # ancestor file not deleted
      child = built[child_dir]
      if child is empty: remove entries[name(child_dir)]
      else: entries[name(child_dir)] = (Tree, id(child), 0)
    # 3. puts
    for e in delta where parent(e.path) == dir and e.op == Put:
      if entries[name(e.path)] is a Tree: fail("file_directory")      # directory not fully deleted
      require entries[name(e.path)] != (e.mode, e.id) else fail("noop_put")
      entries[name(e.path)] = (e.mode, e.id, e.size)
    if dir != "" and entries is empty: built[dir] = EMPTY   # pruned by its parent
    else: built[dir] = Tree(sorted(entries))
  return id(built[""]), non-empty trees in built not already stored
```

### 18.3 Rebase One Changeset

```text
function rebase(c, x):
  p = c.parents[0]
  if x == p: return c
  B, O, T = tree(p), tree(x), tree(c)
  resolved = {}; conflicts = []
  for path in c.delta.paths:
    b, o, t = B[path], O[path], T[path]
    if structural_conflict(O, path, t): conflicts += (path, "file_directory"); continue
    if o == b: resolved[path] = t
    elif t == b or o == t: resolved[path] = o
    else:
      r = merge_entry(b, o, t)                       # Section 8.3 table + content merge
      if r.conflict: conflicts += r
      else: resolved[path] = r.entry                 # may upload a new merged blob
  if conflicts: fail(MERGE_CONFLICT, clean_entries=resolved, conflicts=conflicts)
  delta = [(path, entry) for path, entry in resolved if entry != O[path]]
  if delta is empty: fail(EMPTY_CHANGE)
  return create_changeset_internal(parents=[x], delta=delta, meta=c.meta, change_id=c.change_id)
```

### 18.4 Land

```text
function land(principal, ref, head):
  for attempt in 1..land.max_attempts:
    (tip, v) = refs.read_strong(ref)
    require policy.can_update(principal, ref)
    stack = first_parent_chain(head, stop_when=is_ancestor(_, tip))   # oldest first
    require all(len(m.parents) == 1 for m in stack)
    onto = tip; landed_rows = []; recorded = []
    for m in stack:
      l = landed.get(ref, m.change_id)
      if l and l.source == m: recorded += l; continue
      if l: fail(ALREADY_LANDED, l)
      require all(policy.can_write(principal, p) for p in m.delta.paths)
      r = rebase(m, onto)                            # may fail MERGE_CONFLICT / EMPTY_CHANGE
      landed_rows += (m.change_id, m, r); onto = r
    if not landed_rows: return {target: tip, version: v, landed: recorded}
    try txn:
      update refs set target=onto, version=v+1 where name=ref and version=v   # 0 rows -> mismatch
      insert ref_log(ref, v+1, tip, onto, principal, now, landed_rows)
      insert landed rows                              # unique violation -> retry
      return {target: onto, version: v+1, landed: recorded + landed_rows}
    except (RefVersionMismatch, UniqueViolation):
      sleep(full_jitter(attempt))
  fail(UNAVAILABLE, "land_contention")
```

### 18.5 Is Ancestor

```text
function is_ancestor(a, b):                          # is a an ancestor of (or equal to) b?
  ga = generation(a)
  stack = [b]; seen = {}
  while stack:
    c = pop(stack)
    if c == a: return true
    if c in seen or generation(c) <= ga: continue    # ancestors of a have generation < ga
    seen += c
    push(stack, parents(c))
  return false

function is_first_parent_ancestor(a, b):             # O(log n) via skip pointers
  c = b
  while generation(c) > generation(a):
    c = farthest skip[k] of c with generation(skip[k]) >= generation(a)
  return c == a
```

Skip pointers are valid only for first-parent walks (they jump over merge parents). Full ancestry
uses generation-pruned search. Land stack detection on a linear `main` uses the first-parent form,
falling back to the full form when `main` contains merges.

### 18.6 Authorize Object Read

```text
function can_read_object(principal, id):
  if redactions.contains(id): return REDACTED
  key = (principal, id, policy_epoch)
  if cache has key: return cache[key]
  for path in object_paths(id):                      # usually 1 row; stops at first allow
    if policy.can_read(principal, path): return cache[key] = ALLOW
  return cache[key] = DENY
```

### 18.7 Status

```text
function status(wc):
  base_tree = tree(wc.base)
  for file in walk(wc.root, scope=wc.sparse, skip=".scion", ignore=gitignore):
    e = wc.index.get(file.path)
    if e and e.stat == stat(file) and not racy(e, wc.index.written_at): continue
    h = blake3(read(file))                           # symlinks: hash target
    b = base_tree[file.path]
    if b is absent: report added
    elif b.id != h: report modified
    elif b.mode != mode(file): report mode_changed
    else: wc.index.refresh(file.path, stat(file))
  for path in base_tree files in scope not seen on disk: report deleted
```

### 18.8 Redact and Propagate

```text
function redact(principal, ids, reason, purge_after):
  require policy.can_admin(principal, "redact")
  txn:
    epoch = counters.increment("redaction_epoch")
    for id in ids:
      require objects[id].kind in {Blob, Meta}
      insert redactions(id, Quarantined, reason, principal, now, now + purge_after, epoch)
      update objects set state = quarantined
  async: move_bytes_to_quarantine(ids)
  return {epoch, effective_by: now + redaction.propagation_bound_ms}

daemon loop every redaction_poll_interval_ms or on trailer epoch > local_epoch:
  entries = upstream.ListRedactions(since=local_epoch)
  for e in entries: local_redacted.add(e.id); cache.delete(e.id)
  local_epoch = entries.current_epoch; last_refresh = now
  if now - last_refresh > redaction_max_staleness_ms: mode = forward_all_reads
```

## 19. Test and Validation Matrix

A conforming implementation MUST include tests covering the behaviors in this specification.

Validation profiles:

- `Core Conformance`: deterministic tests REQUIRED for conformance. They run hermetically against
  single-node CockroachDB, MinIO, an in-process server, an in-process daemon, and a fake GitHub API.
- `Extension Conformance`: REQUIRED only for OPTIONAL or deferred features an implementation ships.
- `Scale and Resilience Profile`: environment-dependent load, soak, and chaos runs RECOMMENDED before
  production. Numeric targets are tracked in Appendix B (D-12).

Unless noted, Sections 19.1-19.18 are `Core Conformance`.

### 19.0 Required Test Infrastructure

- `Reference Model`: a small in-memory implementation of Sections 4, 8.2-8.4, and 9.2 written
  independently of the production code, used as the oracle for property and differential tests.
- `Corpus`: hand-built edge-case repos; generated repos (configurable file count, depth, fan-out,
  history length); at least two real open-source git histories for import differential tests.
- `Storage Access Counters`: test-only instrumentation counting metadata row reads, object reads,
  and bytes, used for complexity gates (19.18).
- `Fault Hooks`: injectable failures at every storage call and between every step of Section 5.3.
- `History Recorder`: records concurrent ref operations for linearizability checking.

### 19.1 Encoding and Identifiers

- Golden vectors for every object kind (encoding bytes and ids) are frozen in the repository; any
  change fails CI.
- Round trip `decode(encode(x)) == x` and `encode(decode(b)) == b` for 10^6 random valid objects.
- Decoder rejects: non-minimal varints, overlong lengths, trailing bytes, unsorted or duplicate
  entries, unknown `format_version`, unknown odd extension tags.
- Unknown even extension tags are preserved byte-for-byte through decode/encode and change the id.
- Blob id equals BLAKE3 of raw content regardless of compression level or framing.
- Ids of different kinds with equal digests are distinct and never confused by any API.
- Text form parses and prints round trip; ambiguous short prefixes are rejected with candidates.

### 19.2 Tree Construction and Changeset Validity

- Property: for random trees `T1` and random valid deltas, `apply_delta` equals the reference model,
  and `FileDiff(T1, result) == delta`.
- Each validation subcode in Section 8.1 has at least one failing test: missing blob, size mismatch,
  no-op put, delete of absent path, delete of a directory, file/directory without deletes, path
  rule violations (`.`, `..`, `/`, `0x00`, `.scion` in any case, overlong name, overlong path),
  limits exceeded.
- Deleting the last file in a directory removes the directory (no empty trees); the root may become
  empty.
- File-to-directory and directory-to-file transitions with the required deletes succeed.
- Merge changesets: delta validated against the first parent only.
- Identical `CreateChangeset` requests, sequential and concurrent, return one id; one row exists.
- `expected_root_tree` mismatch is rejected.
- `creator` is set by the server and cannot be supplied by the client.

### 19.3 Storage Layer

- Write ordering: injecting a crash between each step of Section 5.3 never yields a visible
  changeset violating Section 4.5.
- An `objects` row never exists without durable bytes (fault-injected object storage).
- Inline threshold: objects at threshold-1, threshold, and threshold+1 bytes are stored in the
  expected place and read back identically.
- Stale-read miss is retried strongly before returning `NOT_FOUND`.
- Orphan bytes do not affect any API result.

### 19.4 Rebase and Merge Engine

- Every row of the Section 8.3 table, and both structural conflict rules, has a dedicated test.
- Property (against the reference model, 10^6 cases): if `o == b` result is `t`; if `t == b` result
  is `o`; if `o == t` result is `o`; non-overlapping, non-adjacent hunks from both sides both appear;
  overlapping or adjacent differing hunks conflict; identical hunks apply once.
- Binary heuristic: `0x00` at byte 7999 is binary; at byte 8000 is text.
- `merge_max_text_bytes` boundary.
- Unterminated final lines and CRLF content merge byte-exactly (no normalization).
- Rebase determinism: the same `(C, X)` yields the same id across runs, across server processes, and
  after server restart.
- Rebase fast path: `X == P` returns `C` unchanged.
- `EMPTY_CHANGE` when the change is already in `X`.
- Rebase touches only paths in `C.delta`: zero blob reads when `X` changed only unrelated paths
  (asserted with storage access counters).
- Conflict payloads include `clean_entries` and per-conflict `base/ours/theirs` ids.
- No server-stored object ever contains conflict markers produced by the engine (scan all objects
  created during the merge suite).
- `Merge(a, b)` with a unique base matches the reference model; criss-cross histories return
  `AMBIGUOUS_MERGE_BASE`.
- RECOMMENDED differential check: on clean text merges, compare with `git merge-file`; document
  disagreements.

### 19.5 Refs and Land

- CAS: with 1000 concurrent `UpdateRef` calls at the same expected version, exactly one succeeds;
  versions increase by exactly 1; the ref log has no gaps.
- ABA safety: target A -> B -> A; a CAS expecting the first version fails.
- Fast-forward enforcement; non-fast-forward requires flag and `force`.
- Land with N concurrent clients (N = 10, 100, 1000) landing non-overlapping changes: all land;
  final tree contains every delta exactly once; each change id appears exactly once in `landed`.
- Land with overlapping text changes that merge cleanly: all land; final content equals the
  reference model's sequential application in the landed order recorded in the ref log.
- Land with true conflicts: exactly the conflicting lands fail `MERGE_CONFLICT`; others land.
- Idempotency: crash injected after CAS commit and before response; retry returns the recorded
  result and does not create a second ref version.
- Amended change id after landing returns `ALREADY_LANDED`.
- Partially landed stack: lower members landed separately are skipped; upper members land.
- Stack containing a merge changeset is rejected in v1.
- Authorization re-checked on every attempt: a policy change revoking write mid-retry causes
  failure.
- `land.max_attempts` exhaustion returns a retryable error and leaves the ref unchanged.
- No locks: traces from the concurrent suite show no lock or lease acquisition on the land path.
- Linearizability: concurrent `ResolveRef`/`UpdateRef`/`Land` histories with injected server kills
  and network delays pass a linearizability checker (for example Porcupine) with zero violations.

### 19.6 Derived Data

- `commit_graph`: generation and skip pointers equal the reference model for random DAGs (including
  octopus merges from import); `IsAncestor` and `MergeBase` agree with the reference model on 10^5
  random queries.
- `history`: `Log(path)` equals the reference model's TREESAME-simplified history for every path
  in the generated corpus; node reuse rule verified on merges.
- `blame`: equals the reference model for every file in the generated corpus; equals `git blame`
  output on imported corpora for files without renames (documented exceptions otherwise).
- Determinism: two workers deriving the same key concurrently produce byte-identical values; one row
  remains.
- Lazy derivation 10^5 changesets from the nearest computed value completes without recursion depth
  growth (iterative) and writes back intermediate values.
- Eager values exist for every visible changeset (fault injection between derive and insert).
- Version bump: a new `blame` version re-derives on query with an unchanged client binary; old
  version values remain readable.
- Registering a test-only derived type requires no client change and is queryable through the
  generic derived-data query path used by Query RPCs.

### 19.7 RPC Contracts and Error Model

- Every RPC has tests for success, each listed error code it can return, and authentication failure.
- Batch per-item statuses: a batch mixing ok, not_found, denied, redacted returns each correctly
  without failing the batch.
- Every response carries the three required trailers.
- `ErrorDetail.retryable` matches Section 7.2 for every code.
- Unknown request fields are ignored (protobuf forward compatibility); missing required fields fail
  `INVALID_ARGUMENT`.

### 19.8 Tier Conformance

- One read-RPC suite runs unchanged against the server and against a daemon in front of the server;
  per-item results and decompressed payloads are byte-identical for the same principal and epochs.
- Write, query, ref, auth, and admin RPCs through the daemon behave identically to direct calls.
- The daemon never contacts anything other than its configured upstream (network policy test).

### 19.9 Daemon Cache

- Insert verifies digest; a corrupted upstream response is rejected and not cached.
- Bit flip in a cached file is detected on read (with `verify_on_read`), deleted, and re-fetched.
- Temp files are removed on startup; a crash during insert never leaves a visible partial entry.
- Coalescing: 10000 concurrent requests for one missing object produce one upstream request.
- Eviction keeps usage at or below `max_cache_bytes` under a randomized workload; evicted objects are
  re-fetched transparently.
- Ref cache TTL honored; monotonic version reads across TTL expiry and upstream reordering.
- Upstream outage: cached objects keep serving; misses fail `UNAVAILABLE`; recovery resumes service
  without restart.
- Queue overflow returns `RESOURCE_EXHAUSTED`.

### 19.10 Fetch and Working Copy

- Sparse checkout of directory D requests exactly the trees on the path to D and the objects under
  D (asserted object set).
- Resumption: kill the stream at random offsets; the resumed checkout re-fetches only incomplete
  objects and produces a byte-identical working copy.
- Checkout result equals the reference model's materialization (contents, executable bits,
  symlinks) for random trees.
- Path safety: malicious trees (served by a fake server) containing `..`, `.scion`, and symlink
  traversal are refused before any write; pre-existing symlinked parent directories are not
  followed.
- Case collisions on a case-insensitive filesystem are reported, not overwritten.
- `goto` touches only differing paths (mtime unchanged on others); preserves unrelated dirty files;
  aborts on dirty files it would overwrite.
- `status` correctness: randomized edit sequences (create, modify, delete, chmod, symlink swap,
  rename, truncate-and-write, atomic-rename save, edits within the same second as index write)
  match a full rehash every time, including racy-clean cases.
- `status` makes zero server calls with a warm cache (asserted).
- `.gitignore` semantics match `git check-ignore` on a corpus of ignore patterns.
- `commit` uploads only missing blobs and sends a delta equal to the reference model's diff.
- `amend` preserves `change_id`.
- Conflict flow: markers written for text conflicts; side files for other kinds; `resolve` refuses
  remaining markers; continuing commit preserves `change_id`, author, and message; `--abort`
  restores the original.
- Crash injection at every step of `goto`, `commit`, and `rebase` leaves a recoverable
  `pending_operation` that the next command completes or rolls back.
- `sparse add/remove` materializes or removes exactly the affected files; refuses dirty removal.
- Inaccessible subtrees are recorded and excluded from status and commit.
- `--json` output validates against the published schema for every command.

### 19.11 Authentication

- GitHub login against a fake GitHub API: valid token -> principal `github:<login>` with numeric
  id in the token; invalid token -> `UNAUTHENTICATED`.
- Service login: correct secret succeeds; wrong secret fails; the comparison is constant-time
  (code review checklist item plus a test that both paths use the constant-time primitive).
- Expired, malformed, wrong-key, and tampered tokens are rejected by server and daemon.
- Key rotation: tokens signed by an old key still verify while it is listed; removal invalidates
  them.
- Credentials file permissions enforced on write and read.

### 19.12 Access Control

- Policy semantics: longest prefix wins; component-wise matching (`/secrets` vs `/secrets2`);
  independent read and write resolution; nested groups; cycle detection; `github-id:` form; ref
  pattern precedence; default deny.
- Policy reload: valid change applied atomically and epoch changes; invalid change keeps last good
  and surfaces an error; invalid at startup fails.
- Matrix: every read and query RPC x {allowed, denied at `/`, denied at a subdirectory} x {server,
  daemon} returns the specified result.
- Retroactivity: adding a restriction makes previously readable objects that appear only under the
  new prefix unreadable immediately, with no reindex; objects also present at an unrestricted path
  remain readable.
- `HasObjects` returns false for existing objects the caller cannot read and did not upload
  (existence-probe test); duplicate upload of such an object succeeds without leaking.
- Unreferenced uploaded objects are not readable via `GetObjects` by anyone.
- History filtering: `GetChangesets` filtered view, `Log`, `Diff`, `Blame`, `ReadPath` never return
  restricted paths or content to unauthorized principals.
- Daemon: a restricted object cached for principal A is not served to principal B without B's own
  positive decision; policy epoch change drops decisions.
- Write checks: `CreateChangeset` and `Land` reject unwritable paths; ref `update` and `force`
  enforced; admin RPCs require their permissions.

### 19.13 Redaction

- Redacting a blob changes no tree, changeset, or ref id (verify all ids before and after).
- After `effective_by`, server returns `redacted` for the object; a daemon returns `redacted`
  within `redaction_poll_interval_ms` plus one request, and deletes its cached copy.
- Daemon fail-closed: block redaction polling beyond `redaction_max_staleness_ms`; the daemon stops
  serving cached objects and forwards reads.
- `--all-versions` finds every distinct blob a path ever held (against the reference model).
- Meta redaction: `Log` and `show` render `<redacted>`; changeset ids unchanged.
- Re-introducing redacted content by upload or `Put` fails `REDACTED`; carrying the entry forward
  unchanged in later changesets succeeds.
- Merge needing redacted content produces a `redacted` conflict.
- `Unredact` from quarantine restores reads byte-identically; `Unredact` after purge fails.
- Purge deletes object-storage bytes (live and quarantine keys), inline data, and content-bearing
  derived records; non-content-bearing derived values remain and later re-derivation is
  deterministic.
- Working copy: checkout writes placeholders; status treats them as clean; newly redacted clean
  files are replaced on the next mutating command; dirty files untouched and reported.
- Audit log records every redaction action with actor and reason.

### 19.14 Git Import

- Differential fidelity: for every commit of each corpus repository, the Scion tree's file set,
  modes, and contents equal `git ls-tree -r` plus blob contents (modulo documented exclusions).
- Meta preserves author, committer, both timestamps and zones, and message byte-for-byte.
- `git_mapping` resolves both directions for every commit.
- Re-import is idempotent (no new rows); incremental import adds only new commits and
  fast-forwards.
- Octopus merges, root commits, multiple roots, empty commits, mode 100664, and symlinks import
  correctly; gitlinks fail without `--skip-submodules` and are recorded with it.

### 19.15 Configuration

- Unknown keys rejected with the key name; `$VAR` resolution and unresolved-variable errors; every
  default in Section 6.5 applied when a key is omitted.

### 19.16 Failure Injection and Recovery

- Each failure class in Section 16.1 has a test demonstrating the specified behavior and the
  Section 16.2 invariants.

### 19.17 Observability

- Required log fields present on every RPC log event; no token, secret, or file content appears in
  logs under the full test suite (log scan).
- Every metric in Section 15.2 is exported and moves as expected in its suite.
- `/healthz` and `/readyz` reflect dependency state.

### 19.18 Complexity Gates (deterministic)

Using storage access counters, on generated repositories of 10^3, 10^5, and 10^7 files with
equal depth:

- `CreateChangeset` of a 1-file change reads at most `depth + c` trees and writes at most
  `depth + 1` trees, independent of repository size.
- `Rebase` over an unrelated tip reads zero blobs and O(|delta| x depth) trees.
- `Land` of a 1-file change performs a constant number of metadata transactions per attempt.
- Sparse checkout of a fixed directory fetches the same object count at every repository size.
- Clean `status` reads zero objects from upstream and hashes only racily-clean or stat-changed files.
- Eager derivation of a 1-file change creates at most `depth + 1` history nodes.
- Object authorization reads `object_paths` rows only for the requested object.

### 19.19 Scale and Resilience Profile (RECOMMENDED)

- Sustained land throughput and burst behavior at the D-12 targets for 72 hours.
- Checkout latency (warm and cold daemon) at the D-12 targets.
- Daemon hit rate above 99% by request count and by bytes on a production-shaped replay.
- 14-day soak with continuous fault injection (process kills, partitions, storage latency, disk
  full): zero lost writes, zero linearizability violations, zero integrity failures served.
- CockroachDB node and zone loss: writes pause or fail over per store guarantees; no manual repair.
- Disaster recovery: restore metadata and objects from backup into a new environment; every ref and
  changeset verifies.
- Rolling upgrade of server and daemon under load with no client-visible errors.
- Import of the largest available real history; full `blame`/`log` differential pass.

## 20. Implementation Checklist (Definition of Done)

Use the same validation profiles as Section 19:

- Section 20.1 = `Core Conformance`
- Section 20.2 = `Extension Conformance`
- Section 20.3 = `Scale and Resilience Profile`

### 20.1 REQUIRED for Conformance

- Canonical encoding, identifiers, golden vectors (Section 4)
- Object, tree, Meta, and changeset validation, including all Section 8.1 subcodes
- CockroachDB schema and Section 5.3 write ordering
- Object storage layout with inline threshold
- Server-side tree construction in O(|delta| x depth)
- Deterministic rebase and three-way merge (`line-diff3-v1`)
- Lock-free `Land` with idempotent retries and `landed` index
- Version-checked `UpdateRef` with fast-forward enforcement and ref log
- Derived-data framework with `commit_graph`, `history`, `blame`, `path_index`, `git_mapping`
- Query RPCs: `ReadPath`, `ListTree`, `Log`, `Blame`, `Diff`, `IsAncestor`, `MergeBase`, `Resolve`
- `scion-daemon` with verified cache, coalescing, eviction, ref TTL, epoch handling, tier
  conformance
- Client: checkout, sparse, goto, status, commit, amend, rebase with conflict resolution, land, log,
  blame, show, cat, `--json`
- GitHub device-flow login and service principals; signed tokens verified by server and daemon
- `acl.toml` policy with reload; retroactive object authorization; history filtering; existence-probe
  protection
- Redaction lifecycle (quarantine, unredact, purge), propagation bounds, fail-closed daemon,
  placeholders, re-introduction blocking
- `scion-import` with fidelity guarantees
- Structured logs, Section 15.2 metrics, health endpoints
- All Section 19.1-19.18 tests passing

### 20.2 RECOMMENDED Extensions (Not REQUIRED for Conformance)

- Regional cache tier (same RPCs; Section 3.3)
- Virtualized working copies (FUSE on Linux; NFS loopback or FSKit on macOS)
- Filesystem-watcher-backed `status`
- Daemon snapshot bootstrap
- Chunked storage for large blobs behind unchanged ids
- Land policy hooks (review approval, CI status)
- Rename/copy detection derived type; search index derived type
- Object GC for orphans and abandoned drafts
- Path-name redaction via serve-time masking

### 20.3 Operational Validation Before Production (RECOMMENDED)

- Run Section 19.19 with production-like data and topology.
- Verify backup retention meets Section 13.8.
- Verify GitHub OAuth App configuration and service secret distribution in the target environment.
- Exercise the redaction runbook end to end (redact, verify propagation, purge, verify backups
  expire).

## Appendix A. Deferred Work and Known Edge Cases

| Item | v1 behavior | Path forward |
|---|---|---|
| Regional cache tier | Absent | Insert between daemon and server; same RPCs |
| Virtualized working copy | Absent | Tree entries carry sizes so `stat` needs no blob fetch |
| Watcher `status` | Stat-cache walk, O(materialized files) | Add watcher; stat walk remains the correctness fallback |
| Large files | Whole blobs, 100 MiB default limit | BLAKE3 tree hashing allows chunked storage and verified range reads with unchanged ids |
| Huge directories | Single tree object per directory | New tree `format_version` with sharded entries |
| Rename/copy detection | None; merge treats renames as delete + add | Derived type; merge engine version bump |
| Criss-cross merges | `AMBIGUOUS_MERGE_BASE` | Recursive/virtual merge base in a later engine version |
| Merge changesets in land stacks | Rejected | Generalize stack walk |
| Add/add identical-base merges | Conflict | Two-way merge with empty base |
| Log through delete and re-add | History stops at re-creation | Link nodes across deletion in a new `history` version |
| Restricted directory names | Visible in parent tree listings | Serve-time name masking (no id change) |
| Meta of restricted-only changesets | Visible to all readers | Per-changeset meta visibility rule |
| Path-name redaction | Not supported | Serve-time masking of tree entries with server-signed views |
| GitHub login recycling | Policy may reference logins | Prefer `github-id:`; pin ids at first sight |
| GitHub team-based groups | Not supported | Group source adapter |
| Line endings | Bytes stored verbatim | Attribute-driven filters in a later client version |
| Submodules | Rejected or skipped at import | Not planned |
| Orphan and draft GC | None | Mark-and-sweep from refs plus TTL for drafts |
| Multi-repo | One repo row | Schema already repo-keyed |
| Two-way git sync | Import only | Bridge service |
| Windows clients | Unsupported | Path rules already exclude problematic names for POSIX only |

## Appendix B. Decision Records

| Id | Decision | Status |
|---|---|---|
| D-1 | Remove regional cache tier for v1; keep tier-uniform RPCs | Accepted |
| D-2 | Single monorepo; humans, agents, CI; Unix clients | Accepted |
| D-3 | CockroachDB metadata store | Accepted |
| D-4 | Rust, gRPC/protobuf transport | Accepted |
| D-5 | BLAKE3-256, custom canonical binary encoding, zstd | Accepted |
| D-6 | Changeset identity: content hash + stable ChangeId + ref-log version; no wall clock in rebased changesets | Proposed (see below) |
| D-7 | Git defaults for modes, paths, empty dirs, binary detection, line endings, ignore rules; 100 MiB blob limit | Accepted |
| D-8 | Simple deterministic merge (`line-diff3-v1`) with explicit conflict kinds; edge cases in Appendix A | Accepted |
| D-9 | Access policy in `acl.toml`, longest-prefix rules, retroactive via path index | Accepted |
| D-10 | GitHub device-flow login; service principals by shared secret | Accepted |
| D-11 | Redaction by quarantine/purge of Blob and Meta bytes; ids unchanged; permanent blocklist | Accepted |
| D-12 | Scale and latency targets for Section 19.19 | Open |

D-6 detail:

- A changeset id is the hash of its canonical encoding (parents, root tree, meta id, change id,
  delta). It is self-verifying, coordination-free, and safely cacheable everywhere.
- A rebase necessarily changes the id (the parent changes). The `ChangeId` provides stable
  identity across rebase and amend, for review tools and land idempotency.
- Ordering and human-friendly numbering come from the ref log version (`main@12345`). It is
  assigned in the CAS transaction that already serializes the ref, so it adds no global
  coordination.
- No server wall-clock time is hashed into rebased changesets. Rebase is therefore a pure function,
  which makes retries deterministic. Landing time lives in the ref log.
- Meta is a separate object, so author and message remain redactable without changing ids.

## Appendix C. One-Way Door Review

| Commitment | Why it is reversible or bounded |
|---|---|
| Hash algorithm | `algo` byte in every id; a new algorithm coexists with old ids |
| Object formats | `format_version` per object; ignorable/critical extension tags |
| Changeset carries `root_tree` and `delta` | Both fully determined by trees; new versions may add fields via extensions |
| Meta embedded vs referenced | Referenced by id: redaction possible forever |
| Blob identity = hash of whole content | Allows chunked storage, verified range reads, and dedup layers later without id change |
| Wire protocol | Versioned gRPC packages served side by side |
| Two tiers only | Tier-uniform RPCs allow inserting tiers transparently |
| Merge semantics | Results are ordinary changesets; engine is versioned; changing it later affects only future merges |
| Derived data formats | Versioned per type; re-derivable from changesets at any time |
| Policy model | Evaluated at request time over a path index; model can change without data migration |
| Single repo | Every table and key is repo-scoped |
| Tree entry names hashed into tree ids | Residual one-way door: name changes require history rewrite or serve-time masking; masking is designed for (Appendix A) |
| Redaction is irreversible after purge | Intentional; quarantine window provides reversibility |
