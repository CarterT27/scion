# Scion Implementation Plan (v1, Core Conformance)

Status: final, synthesized from three architecture proposals and three judge verdicts.
Base architecture: Proposal A, the consensus winner: layered crates that mirror SPEC 3.2, a mechanical layer check, and a wave-0 freeze of every API as compiling stubs. Grafted onto it:

- From B: importer contract (`import_mode`, eager `git_mapping`), GitHub fields in `ServerInfo`, a `Rebaser` seam, storage-semantics doc, forced txn failures in the in-memory store, load shedding.
- From C: `fsck` oracle, refmodel independence test, contract doc for the merge engine, backup-retention doc, whole-system differential, mock `DeriveCtx`, fake upstream so clients start early.
- From the judges: fixes for every DAG error, file conflict and coverage gap they found.

The machine-readable package list is `docs/PACKAGES.json`. §4 of this file is rendered from it, so the two must stay identical.

## 0. How to use this plan

- Waves run in order: 0, 1, ... 6. Packages within a wave run in parallel, one fresh agent per package. Their file ownership is disjoint.
- Every package brief has four parts:
  - The package entry in `PACKAGES.json`.
  - §2 of this file: the frozen API the package implements or consumes.
  - §3: the testing strategy.
  - §5: the conventions.
- **Definition of done (every package):** all of the following pass in the package's worktree.

  ```
  cargo fmt --all -- --check
  cargo build --workspace --all-targets
  cargo test --workspace
  cargo clippy --workspace --all-targets -- -D warnings
  scripts/check-layers.sh
  ```

  `scripts/check.sh` runs all five. In addition:
  - `git diff --name-only` against the wave base lists only files the package owns.
  - Every frozen signature in §2 is unchanged.
- **Gate G0.** Wave 0 is followed by a contract-review gate. A fresh agent compares the P00 output with SPEC.md and §2. It may fix only compile, consistency or SPEC-mismatch defects, and only in P00-owned files. Every change is logged in `docs/contracts/amendments.md`.

## 1. Architecture overview and crate layout

### 1.1 Layering (SPEC 3.2) mapped to crates

| SPEC level | Crates | Notes |
|---|---|---|
| 1 Object | `scion-core`, `scion-object` | `scion-core` holds the shared vocabulary: errors, ids, modes, principals, clock, limits, fault registry. `scion-object` holds the canonical codec and entities. No IO. |
| 2 Storage | `scion-store`, `scion-store-crdb`, `scion-store-s3` | Traits, in-memory backends, ObjectRepo facade, counters, fault decorators, fsck, backend conformance suites. |
| 3 History | `scion-merge` (pure), `scion-history` | Tree construction, CreateChangeset, commit graph, rebase/merge, refs, lock-free land. |
| 4 Derived | `scion-derived` | Framework, history nodes, blame, path_index, git_mapping, Log(path), all-versions. |
| 5 Service | `scion-proto`, `scion-auth`, `scion-config`, `scion-obs`, `scion-redaction`, `scion-server` | gRPC surface, tokens, configs, observability, redaction mechanics, server binary. |
| 6 Cache | `scion-cache`, `scion-daemon` | Verified disk CAS, coalescing, ref TTL cache, daemon binary. |
| 7 Working copy | `scion-wc`, `scion-cli`, `scion-import` | Working-copy library, `scion` CLI, git importer. |
| 8 Policy | `scion-policy` | A leaf crate (depends only on core/object). Consumed by the service layer only. |
| Test infrastructure (19.0) | `scion-refmodel`, `scion-testkit`, `scion-conformance` | Outside the layering. `scion-refmodel` depends on no scion crate. |

Upward needs are inverted through traits declared in the lower crate:

- `scion-history` declares `EagerHook` (implemented by `scion-derived`), plus `WriteAuthorizer`, `LandAuthorizer`, `LandAuthorizerSource` and `Rebaser`. The authorizer traits are implemented by `scion-server` on top of `scion-policy`.
- `scion-wc::status` reads trees through `TreeLookup`.

The object and storage layers never see policy.

### 1.2 Crates (23)

| Crate | Purpose | Internal deps (normal) |
|---|---|---|
| scion-core | ErrorCode/ScionError/ErrorPayload (7.2), ObjectId/Kind/ChangeId + text forms, Mode, conflict payload types, Principal, Tier, Clock/ManualClock, Limits (6.5), FaultRegistry + checkpoint names, subcode constants, test_support | none |
| scion-object | Canonical encoder/decoder (4.1), Canonical trait, Extensions, Tree/Meta/Changeset/HistoryNode, Name/RepoPath (4.4), RefName, revision grammar (11.8), FlatTree + pure file_diff | core |
| scion-store | MetaStore/BlobStore traits, row types, Storage bundle, MemMetaStore/MemBlobStore, ObjectRepo (5.1, 5.3 step 1, 5.4), Counters + Instrumented decorators (19.0), fsck (4.5 oracle), backend conformance suites (always compiled) | core, object |
| scion-store-crdb | CockroachDB MetaStore (tokio-postgres + deadpool-postgres), migrations, 40001 retry, follower reads | core, object, store |
| scion-store-s3 | S3 BlobStore via `object_store` (aws, path-style) | core, store |
| scion-merge | Myers diff (normative tie-breaking), line-diff3-v1, binary heuristic, pure 8.3 entry classification, markers, unified hunks, blame line origins | core |
| scion-history | History: trees, apply_delta, FileDiff, CreateChangeset, bootstrap, commit graph queries, rebase/Merge, refs/UpdateRef, Land | core, object, store, merge |
| scion-derived | Derived framework (eager hook, lazy iterative, leases), history nodes (kind 0x10), path_index/commit_graph descriptors, git_mapping, blame, Log(path), all_versions | core, object, store, merge, history |
| scion-redaction | Redact/Unredact/Purge mechanics, quarantine byte moves, scheduled purge, derived purge | core, object, store, derived |
| scion-policy | acl.toml parse/validate/evaluate, policy_epoch, hot reload | core, object |
| scion-auth | PASETO v4.public Ed25519 tokens, keyring/rotation, ct_eq, service principals, GitHub client, device flow | core |
| scion-config | Server/daemon/client TOML configs, `$VAR`, unknown-key errors, 6.5 defaults | core |
| scion-obs | JSON logging, RPC log events, path hashing, Prometheus metrics, health router, LogCapture | core |
| scion-proto | `scion.v1` codegen, domain<->proto conversions, Status<->ScionError, epoch trailer layer, ScionClient (unix/https), RawCodec | core, object |
| scion-server | Server library + `scion-server` bin: app wiring, auth interceptor, trailers, rate limit + load shed, RedactionSet, object authz (18.6), all handlers, `testing::InProcessServer` | core, object, store, store-crdb, store-s3, merge, history, derived, redaction, policy, auth, config, obs, proto |
| scion-cache | DiskCache, Coalescer, UpstreamLimiter, RefCache, LocalRedactions, AccessCache | core, object |
| scion-daemon | Daemon library + `scion-daemon` bin: Unix-socket ObjectService from cache, raw forwarding, redaction poll, fail-closed, `testing::InProcessDaemon` | core, object, cache, auth, config, obs, proto |
| scion-wc | Working copy: safe fs, index, status, ignore, state/lock/pending ops, credentials, fetch, checkout/goto/sparse, placeholders, commit/amend/rebase/resolve/land, recovery, `testing` scenarios | core, object, merge, cache, config, proto, auth |
| scion-cli | `scion` bin: clap commands, `--json` + schemas/v1, login | core, object, config, proto, auth, wc |
| scion-import | `scion-import` bin + lib: gix-based importer over RPC | core, object, proto, auth |
| scion-refmodel | Independent oracle (19.0). Third-party deps only: `blake3` | none |
| scion-testkit | Corpus generators, edge-case repos, vendored git bundles, WGL linearizability checker, FakeUpstream, FakeGitHub, TestTls | core, object, proto, refmodel |
| scion-conformance | `publish = false`. TestCluster plus the cross-cutting 19.x suites | all of the above |

Dev-dependency rule: crates that `scion-testkit` depends on must not dev-depend on `scion-testkit`. Those crates are core, object, proto and refmodel. The rule avoids duplicate crate instances in a dependency cycle. Those crates use `scion_core::test_support` for proptest scaling. Every other crate may dev-depend on `scion-testkit`.

### 1.3 Layer allow-table (`scripts/layers.toml`, enforced by `scripts/check-layers.sh`)

`check-layers.sh` reads `cargo metadata --format-version 1`. For every workspace crate, each normal and build dependency on another workspace crate must appear in that crate's allowed list. The allowed lists are exactly the "Internal deps" column of §1.2. Additional hard checks:

1. `scion-refmodel` has zero scion-* dependencies of any kind, including dev.
2. None of core, object, store, merge, history or derived depends (normal) on policy, auth, server, proto, config or obs.
3. Dev-dependencies may point anywhere except where the §1.2 dev-dependency rule forbids it.

The script exits non-zero and prints the offending edge.

### 1.4 Server wiring (built by `ServerApp::build`)

```
Storage(meta: Crdb|Mem, blobs: S3|Mem; wrapped by Instrumented decorators; counters; faults)
  -> ObjectRepo(prefix, inline threshold, zstd level, LRU of decoded trees/metas)
  -> History(objects, cfg, eager = DerivedEngine, clock).with_land_observer(metrics)   # bootstrap() at startup
  -> DerivedEngine(objects, Registry::v1_default())                    # implements EagerHook
  -> Redactor(storage, objects, derived)    RedactionSet(meta)          # refresher task
  -> PolicyHandle(acl.toml, watcher)   Keyring/Verifier   ServicePrincipals   GitHubClient
  -> ObjectAuthorizer(storage, policy, redactions)
  -> tonic Router: 6 services (grpc.rs) -> handlers/* ; layers: request-id, auth interceptor,
     load shed + concurrency limit, per-principal governor, RPC log/metrics, EpochTrailerLayer
  -> admin listener: /metrics /healthz /readyz
```

The daemon wiring is: Unix listener -> ObjectService served by scion-daemon (DiskCache + Coalescer + UpstreamLimiter + RefCache + AccessCache + LocalRedactions) -> upstream `ScionClient`. Every other gRPC path goes through the RawCodec passthrough.

### 1.5 Repository layout

```
Cargo.toml  Cargo.lock  rust-toolchain.toml  rustfmt.toml  clippy.toml  .cargo/config.toml
proto/scion/v1/scion.proto
crates/<23 crates>/{Cargo.toml, src/, tests/}
testdata/golden/objects/*.json   testdata/golden/diff/*.json
corpus/git/*.bundle (+ LICENSE notes)
schemas/v1/*.schema.json
scripts/{check.sh, check-layers.sh, layers.toml, test-external.sh}
docker/compose.test.yml
docs/{PLAN.md, PACKAGES.json, implementation-defined.md, testing.md}
docs/contracts/{line-diff3-v1.md, storage-semantics.md, amendments.md}
docs/operations/backup-retention.md
.github/workflows/ci.yml
```

## 2. Frozen public API contracts

### 2.0 Freeze rules, stub conventions, amendment protocol

- P00 writes every crate's `Cargo.toml` (all internal and third-party deps pre-declared, `workspace = true`), every `lib.rs` (module declarations and re-exports), and every module file listed in §4 ownership. Each item below exists with rustdoc and the exact signature shown.
- Stub bodies:
  - Behaviour methods are `todo!("<package id>")`.
  - Assembly constructors are real in P00. Assembly constructors are `new` functions that only store arguments or build empty in-memory state. Examples: `History::new`, `DerivedEngine::new`, `Redactor::new`, `ObjectRepo::new`, `Storage::new`, `MemMetaStore::new`, `MemBlobStore::new`, `Registry::empty`. Plain field accessors are also real.
  - Startup hook functions that the app calls are no-ops in P00, not `todo!()`. Example: `handlers::admin::spawn_background`.
  - gRPC handler stubs return `Err(Status::unimplemented("<package id>"))`.
- Signatures below are frozen. "..." inside a struct means private fields chosen by the implementing package. Packages MAY add private items, new private submodules under their own module (e.g. `create.rs` plus `create/*.rs`), and *additive* `pub` items inside files they own. They MUST NOT change or remove a frozen item.
- Amendments: a package that needs a breaking or cross-crate change records an amendment request in its final report. The orchestrator applies queued amendments between waves as a serialized patch and logs them in `docs/contracts/amendments.md`.
- Manifests: no package edits any `Cargo.toml` or `Cargo.lock`. A needed dependency is an amendment.
- Async traits use `#[async_trait::async_trait]`, so they are object-safe (`Arc<dyn MetaStore>`). `bytes::Bytes` is used for payloads.

### 2.1 scion-core (fully implemented by P00)

```rust
// error.rs
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCode { NotFound, PermissionDenied, Unauthenticated, InvalidArgument, TooLarge, Integrity,
    Redacted, MergeConflict, AmbiguousMergeBase, RefVersionMismatch, NotFastForward, AlreadyLanded,
    EmptyChange, ResourceExhausted, Unavailable }
impl ErrorCode {
    pub const ALL: [ErrorCode; 15];
    pub fn as_str(self) -> &'static str;            // "NOT_FOUND", ... exactly as SPEC 7.2
    pub fn parse(s: &str) -> Option<Self>;
    pub fn grpc_code(self) -> GrpcCode;             // SPEC 7.2 table
    pub fn retryable(self) -> bool;                 // true only for INTEGRITY, REF_VERSION_MISMATCH, RESOURCE_EXHAUSTED, UNAVAILABLE
}
#[repr(i32)] #[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GrpcCode { InvalidArgument = 3, NotFound = 5, AlreadyExists = 6, PermissionDenied = 7,
    ResourceExhausted = 8, FailedPrecondition = 9, Aborted = 10, Unimplemented = 12, Internal = 13,
    Unavailable = 14, DataLoss = 15, Unauthenticated = 16 }

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{}: {message}", code.as_str())]
pub struct ScionError { pub code: ErrorCode, pub message: String, pub payload: ErrorPayload }
#[derive(Clone, Debug, PartialEq, Default)]
pub enum ErrorPayload {
    #[default] None,
    Subcode(String),                                            // INVALID_ARGUMENT / UNAVAILABLE subcodes (subcode.rs)
    Limit { name: String, limit: u64, actual: u64 },            // TOO_LARGE
    MergeConflict(Box<ConflictReport>),                         // MERGE_CONFLICT
    RefMismatch { current_version: u64, current_target: Option<ObjectId> }, // REF_VERSION_MISMATCH
    AlreadyLanded { source: ObjectId, result: ObjectId, version: u64 },     // ALREADY_LANDED
    Candidates(Vec<ObjectId>),                                  // ambiguous prefix
}
impl ScionError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self;
    pub fn with_payload(self, p: ErrorPayload) -> Self;
    pub fn invalid(subcode: &str, message: impl Into<String>) -> Self;
    pub fn too_large(name: &str, limit: u64, actual: u64) -> Self;
    pub fn not_found(m: impl Into<String>) -> Self;   pub fn permission_denied(m: impl Into<String>) -> Self;
    pub fn unauthenticated(m: impl Into<String>) -> Self; pub fn unavailable(subcode: &str, m: impl Into<String>) -> Self;
    pub fn resource_exhausted(m: impl Into<String>) -> Self;
    pub fn redacted(id: &ObjectId) -> Self;           pub fn integrity(id: &ObjectId) -> Self;
    pub fn subcode(&self) -> Option<&str>;
    pub fn retryable(&self) -> bool;                  // == self.code.retryable()
}
pub type Result<T, E = ScionError> = std::result::Result<T, E>;

// subcode.rs: frozen string constants (the payload.subcode values)
pub mod subcode {
  // decoding (4.1)
  pub const NON_MINIMAL_VARINT: &str = "non_minimal_varint"; pub const VARINT_OVERFLOW: &str = "varint_overflow";
  pub const LENGTH_OVERRUN: &str = "length_overrun"; pub const TRAILING_BYTES: &str = "trailing_bytes";
  pub const UNSORTED: &str = "unsorted"; pub const DUPLICATE: &str = "duplicate";
  pub const UNKNOWN_FORMAT_VERSION: &str = "unknown_format_version";
  pub const UNKNOWN_CRITICAL_EXTENSION: &str = "unknown_critical_extension";
  pub const INVALID_MODE: &str = "invalid_mode"; pub const INVALID_KIND: &str = "invalid_kind";
  pub const INVALID_ALGO: &str = "invalid_algo"; pub const EMPTY_TREE: &str = "empty_tree";
  pub const BAD_ID_TEXT: &str = "bad_id_text";
  // CreateChangeset (8.1), in validation order
  pub const MISSING_PARENT: &str = "missing_parent"; pub const DUPLICATE_PARENT: &str = "duplicate_parent";
  pub const PARENTLESS_NOT_ALLOWED: &str = "parentless_not_allowed";
  pub const DELTA_UNSORTED: &str = "delta_unsorted"; pub const DELTA_DUPLICATE: &str = "delta_duplicate";
  pub const INVALID_PATH: &str = "invalid_path";
  pub const MISSING_BLOB: &str = "missing_blob"; pub const SIZE_MISMATCH: &str = "size_mismatch";
  pub const DELETE_ABSENT: &str = "delete_absent"; pub const DELETE_NOT_FILE: &str = "delete_not_file";
  pub const NOOP_PUT: &str = "noop_put"; pub const FILE_DIRECTORY: &str = "file_directory";
  pub const ROOT_TREE_MISMATCH: &str = "root_tree_mismatch"; pub const MISSING_FIELD: &str = "missing_field";
  // TOO_LARGE limit names: "max_parents","max_delta_entries","max_blob_bytes","max_message_bytes" (Limit payload)
  // refs / land / rebase
  pub const INVALID_REF_NAME: &str = "invalid_ref_name"; pub const MULTI_PARENT: &str = "multi_parent";
  pub const MERGE_IN_STACK: &str = "merge_in_stack"; pub const LAND_CONTENTION: &str = "land_contention";
  pub const REF_EXISTS: &str = "ref_exists";
  // revisions (11.8)
  pub const INVALID_REVISION: &str = "invalid_revision"; pub const AMBIGUOUS_PREFIX: &str = "ambiguous_prefix";
  // redaction (13)
  pub const NOT_REDACTABLE: &str = "not_redactable"; pub const NOT_QUARANTINED: &str = "not_quarantined";
  // infrastructure
  pub const INJECTED: &str = "injected"; pub const INJECTED_CRASH: &str = "injected_crash";
  pub const TXN_RETRY: &str = "txn_retry"; pub const STORE_UNAVAILABLE: &str = "store_unavailable";
  pub const UPSTREAM_UNAVAILABLE: &str = "upstream_unavailable";
}

// id.rs
pub const ALGO_BLAKE3: u8 = 0x01;
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind { Blob, Tree, Changeset, Meta, Derived(u8) /* 0x10..=0x7F */ }
impl Kind { pub fn byte(self) -> u8; pub fn from_byte(b: u8) -> Result<Self>; pub fn letter(self) -> char /* b t c m x */;
            pub fn index(self) -> usize /* Blob0 Tree1 Changeset2 Meta3 Derived4, for counters */ }
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId([u8; 34]);                           // algo || kind || digest
impl ObjectId {
    pub fn new(kind: Kind, digest: [u8; 32]) -> Self;
    pub fn from_bytes(b: &[u8]) -> Result<Self>;          // validates len, algo, kind
    pub fn as_bytes(&self) -> &[u8; 34];
    pub fn kind(&self) -> Kind;  pub fn digest(&self) -> &[u8; 32];
    pub fn hash_blob(raw: &[u8]) -> Self;                 // BLAKE3(raw), kind Blob
    pub fn hash_encoded(kind: Kind, canonical: &[u8]) -> Self;
    pub fn hex(&self) -> String;                          // 64 lowercase hex of digest
    pub fn to_text(&self) -> String;                      // e.g. "c1" + hex; derived 0x10 -> "x1"+hex
    pub fn parse_text(s: &str) -> Result<Self>;           // INVALID_ARGUMENT/bad_id_text
}
// Display, Debug = text form; FromStr; serde Serialize/Deserialize as text form.
pub fn empty_tree_id() -> ObjectId;                       // hash_encoded(Tree, [0x01, 0x00, 0x00])
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChangeId(pub [u8; 16]);
impl ChangeId { pub fn random() -> Self; pub fn to_hex(&self) -> String; pub fn parse_hex(s: &str) -> Result<Self>; }

// mode.rs
#[repr(u8)] #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Mode { File = 1, Executable = 2, Symlink = 3, Tree = 4 }
impl Mode { pub fn from_u8(v: u8) -> Result<Self>; pub fn is_file_like(self) -> bool; pub fn git_mode(self) -> u32; }

// conflict.rs (payload types; paths are raw bytes so core stays below scion-object)
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)] pub struct EntryInfo { pub mode: Mode, pub id: ObjectId, pub size: u64 }
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ConflictKind { AddAdd, ModifyDelete, Symlink, Binary, Redacted, FileDirectory, Content }
#[derive(Clone, Debug, PartialEq, Eq)] pub struct ConflictHunk { pub base: Range<u32>, pub ours: Range<u32>, pub theirs: Range<u32> }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict { pub path: Vec<u8>, pub kind: ConflictKind, pub base: Option<EntryInfo>,
                      pub ours: Option<EntryInfo>, pub theirs: Option<EntryInfo>, pub hunks: Vec<ConflictHunk> }
#[derive(Clone, Debug, PartialEq, Eq)] pub struct CleanEntry { pub path: Vec<u8>, pub entry: Option<EntryInfo> }
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ConflictReport { pub clean_entries: Vec<CleanEntry>, pub conflicts: Vec<Conflict> }

// principal.rs
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Principal { GitHub { login: String, id: Option<u64> }, Service(String) }
impl Principal { pub fn parse(s: &str) -> Result<Self>;      // "github:<login>" | "service:<name>"
                 pub fn id_string(&self) -> String; }          // Display == id_string(); GitHub id is not in the string
#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub enum Tier { Server, Daemon }  // as_str(): "server" | "daemon"

// clock.rs
pub trait Clock: Send + Sync + 'static { fn now(&self) -> SystemTime; }
pub struct SystemClock;                                   // impl Clock
pub struct ManualClock { .. }
impl ManualClock { pub fn new(start: SystemTime) -> Arc<Self>; pub fn advance(&self, d: Duration); pub fn set(&self, t: SystemTime); }

// limits.rs  (serde Deserialize with #[serde(default, deny_unknown_fields)])
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits { pub max_blob_bytes: u64 /*104857600*/, pub max_delta_entries: u64 /*100000*/,
    pub max_parents: u64 /*16*/, pub max_message_bytes: u64 /*1048576*/, pub merge_max_text_bytes: u64 /*10485760*/ }
impl Default for Limits;   // SPEC 6.5

// fault.rs : 19.0 Fault Hooks (named injection points; inert unless armed; unarmed check = one relaxed atomic load)
#[derive(Clone, Debug)]
pub enum FaultAction { Error(ErrorCode), Crash, Delay(Duration), FailAfter }
// Error: return ScionError(code, subcode "injected") before the effect.
// Crash: return UNAVAILABLE/injected_crash and latch every later check() on this registry into failure until reset() (models a dead process).
// FailAfter: decorators perform the effect, then return UNAVAILABLE/injected (models "commit happened, response lost").
pub struct FaultRegistry { .. }
impl FaultRegistry {
    pub fn new() -> Arc<Self>;
    pub fn arm(&self, point: &str, action: FaultAction, times: Option<u32> /* None = always */);
    pub fn disarm_all(&self);  pub fn reset(&self);   // reset clears the crash latch too
    pub fn hits(&self, point: &str) -> u64;           // counts every check of that point, armed or not
    pub async fn check(&self, point: &str) -> Result<()>;
    pub fn armed_after(&self, point: &str) -> bool;   // used by decorators for FailAfter
}
pub mod points {  // checkpoint names (5.3 steps, land, redaction, client); decorators add "meta.<method>.before|after", "blob.<method>.before|after"
  pub const UPLOAD_AFTER_BYTES: &str = "upload.after_bytes";      pub const UPLOAD_AFTER_ROW: &str = "upload.after_row";
  pub const CREATE_AFTER_VALIDATE: &str = "create.after_validate"; pub const CREATE_AFTER_TREES: &str = "create.after_trees";
  pub const CREATE_AFTER_META: &str = "create.after_meta";         pub const CREATE_AFTER_DERIVE: &str = "create.after_derive";
  pub const CREATE_AFTER_COMMIT: &str = "create.after_commit";
  pub const LAND_AFTER_REBASE: &str = "land.after_rebase";         pub const LAND_AFTER_COMMIT: &str = "land.after_commit";
  pub const REDACT_AFTER_COMMIT: &str = "redact.after_commit";     pub const QUARANTINE_AFTER_COPY: &str = "quarantine.after_copy";
  pub const PURGE_AFTER_BYTES: &str = "purge.after_bytes";
}
pub mod trace { pub const LOCK_TARGET: &str = "scion::lock"; }  // every lease/lock acquisition emits a span with this target

// test_support.rs
pub fn proptest_cases(default: u32) -> u32;   // SCION_PROPTEST_CASES overrides; full spec counts live in #[ignore] tests
pub fn full_mode() -> bool;                   // SCION_FULL=1
```

### 2.2 scion-object (canonical encoding API, entities)

```rust
pub mod enc {
  pub struct Encoder { .. }
  impl Encoder { pub fn new() -> Self; pub fn u8(&mut self, v: u8); pub fn uvarint(&mut self, v: u64);
      pub fn svarint(&mut self, v: i64); pub fn bytes(&mut self, b: &[u8]); pub fn raw(&mut self, b: &[u8]);
      pub fn id(&mut self, id: &ObjectId); pub fn finish(self) -> Vec<u8>; }
  pub struct Decoder<'a> { .. }
  impl<'a> Decoder<'a> { pub fn new(b: &'a [u8]) -> Self; pub fn u8(&mut self) -> Result<u8>;
      pub fn uvarint(&mut self) -> Result<u64>;     // rejects non-minimal and > 10 bytes / overflow
      pub fn svarint(&mut self) -> Result<i64>; pub fn bytes(&mut self) -> Result<&'a [u8]>; // rejects overrun
      pub fn fixed<const N: usize>(&mut self) -> Result<[u8; N]>; pub fn id(&mut self) -> Result<ObjectId>;
      pub fn list_len(&mut self) -> Result<usize>;   // count bounded by remaining bytes
      pub fn option_tag(&mut self) -> Result<bool>;  // 0|1, else invalid
      pub fn remaining(&self) -> usize; pub fn finish(self) -> Result<()>; } // TRAILING_BYTES
}
pub trait Canonical: Sized {
    const KIND: Kind;
    fn encode_into(&self, e: &mut enc::Encoder);
    fn decode_from(d: &mut enc::Decoder<'_>) -> Result<Self>;
    fn encode(&self) -> Vec<u8> { /* provided */ }
    fn decode(b: &[u8]) -> Result<Self> { /* provided: decode_from + finish */ }
    fn id(&self) -> ObjectId { ObjectId::hash_encoded(Self::KIND, &self.encode()) }
}
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Extensions(Vec<(u64, Vec<u8>)>);   // strictly ascending tags; decode rejects odd tags not in `known_odd`
impl Extensions { pub fn new() -> Self; pub fn get(&self, tag: u64) -> Option<&[u8]>;
    pub fn insert(&mut self, tag: u64, value: Vec<u8>) -> Result<()>; pub fn iter(&self) -> impl Iterator<Item=(u64, &[u8])>;
    pub fn decode_with(d: &mut enc::Decoder<'_>, known_odd: &[u64]) -> Result<Self>; }
pub const META_EXT_GIT_COMMIT: u64 = 2;        // SPEC 4.3.3 (ignorable)
pub const META_EXT_SKIPPED_GITLINKS: u64 = 4;  // implementation-defined (ignorable): list<(path bytes, sha 20 bytes)>

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)] pub struct Name(Vec<u8>);
impl Name { pub fn new(b: &[u8]) -> Result<Self>; pub fn as_bytes(&self) -> &[u8]; }   // 4.4 rules, INVALID_ARGUMENT/invalid_path
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)] pub struct RepoPath(Vec<u8>); // bytewise Ord
impl RepoPath { pub fn parse(b: &[u8]) -> Result<Self>; pub fn root() -> Self; pub fn is_root(&self) -> bool;
    pub fn as_bytes(&self) -> &[u8]; pub fn parent(&self) -> Option<RepoPath>; pub fn file_name(&self) -> Option<&[u8]>;
    pub fn components(&self) -> impl Iterator<Item=&[u8]>; pub fn ancestors(&self) -> Vec<RepoPath> /* root first, excludes self */;
    pub fn is_ancestor_of(&self, other: &RepoPath) -> bool /* strict, component-wise */; pub fn join(&self, n: &Name) -> Result<RepoPath>; }

#[derive(Clone, Debug, PartialEq, Eq)] pub struct TreeEntry { pub name: Name, pub mode: Mode, pub id: ObjectId, pub size: u64 }
#[derive(Clone, Debug, PartialEq, Eq, Default)] pub struct Tree { pub entries: Vec<TreeEntry>, pub extensions: Extensions }
impl Canonical for Tree { const KIND: Kind = Kind::Tree; .. }   // validates order, mode/kind agreement, size 0 for mode 4
impl Tree { pub fn empty() -> Self; pub fn get(&self, name: &[u8]) -> Option<&TreeEntry>; pub fn validate_non_root(&self) -> Result<()>; }

#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub struct Timestamp { pub seconds: i64, pub tz_offset_minutes: i64 }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta { pub author: Vec<u8>, pub author_time: Timestamp, pub committer: Option<Vec<u8>>,
    pub committer_time: Option<Timestamp>, pub creator: Vec<u8>, pub message: Vec<u8>, pub extensions: Extensions }
impl Canonical for Meta { const KIND: Kind = Kind::Meta; .. }

#[derive(Clone, Debug, PartialEq, Eq)] pub enum DeltaOp { Put { mode: Mode /*1..=3*/, id: ObjectId, size: u64 }, Delete }
#[derive(Clone, Debug, PartialEq, Eq)] pub struct DeltaEntry { pub path: RepoPath, pub op: DeltaOp }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changeset { pub parents: Vec<ObjectId>, pub root_tree: ObjectId, pub meta: ObjectId,
    pub change_id: ChangeId, pub delta: Vec<DeltaEntry>, pub extensions: Extensions }
impl Canonical for Changeset { const KIND: Kind = Kind::Changeset; .. }
impl Changeset { pub fn validate_shape(&self, limits: &Limits) -> Result<()>; } // parent uniqueness/count, delta sorted/unique/limit, path rules

#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub enum NodeKind { File = 1, Dir = 2 }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryNode { pub kind: NodeKind, pub content: ObjectId, pub changeset: ObjectId, pub parents: Vec<ObjectId>,
    pub children: Vec<(Name, ObjectId)> /* Dir only, ascending */, pub extensions: Extensions }
impl Canonical for HistoryNode { const KIND: Kind = Kind::Derived(0x10); .. }

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)] pub struct RefName(String);
impl RefName { pub fn parse(s: &str) -> Result<Self> /* INVALID_ARGUMENT/invalid_ref_name */; pub fn as_str(&self) -> &str;
               pub fn main() -> Self; }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevBase { Id(ObjectId), IdPrefix(String) /* "c1"+8..63 hex */, Name(String) /* ref name, or bare hex>=8 tried as prefix */,
                   RefAt(RefName, u64), Git(String /* 40 hex */) }
#[derive(Clone, Debug, PartialEq, Eq)] pub enum RevSuffix { Ancestor(u32) /* ~n */, Parent(u32) /* ^k, k>=1 */ }
#[derive(Clone, Debug, PartialEq, Eq)] pub struct Revision { pub base: RevBase, pub suffixes: Vec<RevSuffix> }
pub fn parse_revision(s: &str) -> Result<Revision>;  // INVALID_ARGUMENT/invalid_revision

pub type FlatTree = BTreeMap<RepoPath, EntryInfo>;   // file-level entries only (modes 1-3)
pub fn file_diff(a: &FlatTree, b: &FlatTree) -> Vec<DeltaEntry>;   // SPEC 4.3.4 FileDiff, ascending by path
```

### 2.3 scion-store (storage backends)

```rust
// types.rs
#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub struct RepoId(pub i64);
#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub enum Consistency { Strong, Stale }
#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub enum ObjectState { Live, Quarantined, Purged }
#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub enum InsertOutcome { Inserted, AlreadyExists }
#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub enum Area { Live, Quarantine }
#[derive(Clone, Debug, PartialEq)] pub struct ObjectRow { pub id: ObjectId, pub kind: Kind, pub size: u64, pub stored_size: u64,
    pub inline_data: Option<Bytes> /* canonical bytes, uncompressed */, pub state: ObjectState, pub created_at: SystemTime }
#[derive(Clone, Debug, PartialEq)] pub struct ObjectPathRow { pub object: ObjectId, pub path: RepoPath, pub first_changeset: ObjectId, pub at: SystemTime }
#[derive(Clone, Debug, PartialEq)] pub struct ChangesetRow { pub id: ObjectId, pub encoded: Bytes, pub generation: u64,
    pub skip: Vec<ObjectId>, pub change_id: ChangeId, pub created_by: String, pub created_at: SystemTime }
#[derive(Clone, Debug, PartialEq)] pub struct ChangesetBundle { pub row: ChangesetRow, pub parents: Vec<ObjectId>,
    pub object_paths: Vec<ObjectPathRow>, pub derived: Vec<DerivedRow> }
#[derive(Clone, Debug, PartialEq)] pub struct RefRow { pub name: RefName, pub target: ObjectId, pub version: u64, pub updated_at: SystemTime }
#[derive(Clone, Debug, PartialEq, Eq)] pub struct LandedEntry { pub change_id: ChangeId, pub source: ObjectId, pub result: ObjectId }
#[derive(Clone, Debug, PartialEq)] pub struct RefLogRow { pub name: RefName, pub version: u64, pub old_target: Option<ObjectId>,
    pub new_target: Option<ObjectId>, pub principal: String, pub at: SystemTime, pub landed: Vec<LandedEntry> }
#[derive(Clone, Debug, PartialEq)] pub struct LandedRow { pub ref_name: RefName, pub change_id: ChangeId, pub version: u64,
    pub source: ObjectId, pub result: ObjectId }
#[derive(Clone, Debug, PartialEq)] pub enum RefExpect { Absent, Version(u64) }
#[derive(Clone, Debug, PartialEq)]
pub struct RefCas { pub name: RefName, pub expect: RefExpect, pub new_target: Option<ObjectId> /* None = delete */,
    pub principal: String, pub at: SystemTime, pub landed: Vec<LandedEntry>, pub audit: Option<AuditRow> }
#[derive(Clone, Debug, PartialEq)]
pub enum CasOutcome { Committed(Option<RefRow>) /* None after delete */, VersionMismatch { current: Option<RefRow> },
                      LandedConflict { existing: LandedRow } }
#[derive(Clone, Debug, PartialEq, Eq, Hash)] pub struct DerivedKey { pub ty: String, pub version: u32, pub key: Bytes }
#[derive(Clone, Debug, PartialEq)] pub struct DerivedRow { pub key: DerivedKey, pub value: Option<Bytes>, pub storage_key: Option<String>,
    pub source_object: Option<ObjectId> /* content-bearing records: the source id (9.1) */, pub at: SystemTime }
#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub enum RedactionState { Quarantined, Purged }
#[derive(Clone, Debug, PartialEq)] pub struct RedactionRow { pub object: ObjectId, pub state: RedactionState, pub reason: String,
    pub actor: String, pub created_at: SystemTime, pub purge_after: SystemTime, pub epoch: u64 }
#[derive(Clone, Debug, PartialEq)] pub struct RedactTxn { pub ids: Vec<ObjectId>, pub reason: String, pub actor: String,
    pub at: SystemTime, pub purge_after: SystemTime }
#[derive(Clone, Debug, PartialEq)] pub struct AuditRow { pub at: SystemTime, pub actor: String, pub action: String,
    pub target: String, pub reason: String, pub detail: String }
#[derive(Clone, Debug, Default, PartialEq, Eq)] pub struct TableCounts { pub objects: usize, pub changesets: usize, pub refs: usize,
    pub ref_log: usize, pub landed: usize, pub derived: usize, pub object_paths: usize, pub redactions: usize }

// meta.rs : every method is exactly ONE serializable transaction (docs/contracts/storage-semantics.md).
// Infrastructure failures -> ScionError UNAVAILABLE (subcode store_unavailable | txn_retry | injected*). Never partial effects.
#[async_trait]
pub trait MetaStore: Send + Sync + 'static {
    fn repo_id(&self) -> RepoId;
    async fn ping(&self) -> Result<()>;
    // objects
    async fn get_objects(&self, ids: &[ObjectId], c: Consistency) -> Result<Vec<Option<ObjectRow>>>;
    async fn insert_object(&self, row: ObjectRow) -> Result<InsertOutcome>;                    // ON CONFLICT DO NOTHING
    async fn record_upload(&self, id: &ObjectId, principal: &str, at: SystemTime) -> Result<()>;
    async fn has_upload(&self, id: &ObjectId, principal: &str) -> Result<bool>;
    async fn object_paths(&self, id: &ObjectId, limit: usize) -> Result<Vec<ObjectPathRow>>;  // rows for this id only, ascending path
    // changesets
    async fn get_changesets(&self, ids: &[ObjectId], c: Consistency) -> Result<Vec<Option<ChangesetRow>>>;
    async fn changesets_by_prefix(&self, hex_prefix: &str, limit: usize) -> Result<Vec<ObjectId>>;
    async fn children(&self, parent: &ObjectId) -> Result<Vec<ObjectId>>;
    async fn has_root_changeset(&self) -> Result<bool>;
    async fn commit_changeset(&self, b: ChangesetBundle) -> Result<InsertOutcome>;  // all rows ON CONFLICT DO NOTHING, independently
    // refs (always strong)
    async fn get_ref(&self, name: &RefName) -> Result<Option<RefRow>>;
    async fn list_refs(&self) -> Result<Vec<RefRow>>;
    async fn cas_ref(&self, cas: RefCas) -> Result<CasOutcome>;  // version check + ref_log + landed + audit in one txn
    async fn ref_log(&self, name: &RefName, from_version: u64, limit: usize) -> Result<Vec<RefLogRow>>;
    async fn get_landed(&self, name: &RefName, change_id: &ChangeId) -> Result<Option<LandedRow>>;
    // derived
    async fn get_derived(&self, k: &DerivedKey, c: Consistency) -> Result<Option<DerivedRow>>;
    async fn put_derived(&self, rows: Vec<DerivedRow>) -> Result<()>;                          // ON CONFLICT DO NOTHING
    async fn delete_derived_by_source(&self, types: &[String], source: &ObjectId) -> Result<u64>;
    async fn try_lease(&self, k: &DerivedKey, holder: &str, ttl: Duration, now: SystemTime) -> Result<bool>;
    async fn release_lease(&self, k: &DerivedKey, holder: &str) -> Result<()>;
    // redaction (13.2)
    async fn redact(&self, t: RedactTxn) -> Result<u64>;      // epoch+1, insert rows (stamped), objects.state=quarantined
    async fn unredact(&self, ids: &[ObjectId], audit: AuditRow) -> Result<u64>; // all must be Quarantined else INVALID_ARGUMENT/not_quarantined; epoch+1
    async fn mark_purged(&self, ids: &[ObjectId], audit: AuditRow) -> Result<()>; // clears inline_data, state=purged
    async fn redaction_epoch(&self) -> Result<u64>;
    async fn list_redactions(&self, since_epoch: u64) -> Result<(Vec<RedactionRow>, u64 /* current epoch */)>;
    async fn due_purges(&self, now: SystemTime, limit: usize) -> Result<Vec<ObjectId>>;
    // audit (17; extra table, not identity-bearing)
    async fn append_audit(&self, row: AuditRow) -> Result<()>;
    async fn audit_log(&self, limit: usize) -> Result<Vec<AuditRow>>;
}

// blob.rs
#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    async fn put(&self, key: &str, bytes: Bytes) -> Result<()>;
    async fn get(&self, key: &str) -> Result<Option<Bytes>>;
    async fn copy(&self, from: &str, to: &str) -> Result<()>;
    async fn delete(&self, key: &str) -> Result<()>;         // idempotent
    async fn exists(&self, key: &str) -> Result<bool>;
    async fn list(&self, prefix: &str) -> Result<Vec<String>>;
}

// storage.rs
#[derive(Clone)]
pub struct Storage { pub meta: Arc<dyn MetaStore>, pub blobs: Arc<dyn BlobStore>, pub counters: Arc<Counters>, pub faults: Arc<FaultRegistry> }
impl Storage {
    pub fn new(meta: Arc<dyn MetaStore>, blobs: Arc<dyn BlobStore>, faults: Arc<FaultRegistry>) -> Self; // wraps both in Instrumented*
    pub fn in_memory(opts: MemOptions) -> (Self, MemHandles);
}
#[derive(Clone)] pub struct MemHandles { pub meta: Arc<MemMetaStore>, pub blobs: Arc<MemBlobStore> }

// mem_meta.rs / mem_blob.rs : faithful hermetic backends
#[derive(Clone, Debug)] pub struct MemOptions { pub repo_name: String, pub stale_lag_commits: usize /* 0: stale == strong */,
    pub fail_txn_every: Option<u32> /* every Nth txn fails UNAVAILABLE/txn_retry with no effect */, pub seed: u64 }
impl Default for MemOptions;
pub struct MemMetaStore { .. }   // one parking_lot::Mutex over `im` persistent maps; ring of last K committed snapshots for Stale
impl MemMetaStore { pub fn new(opts: MemOptions) -> Self; pub fn set_stale_lag(&self, commits: usize);
    pub fn advance_stale_view(&self); pub fn table_counts(&self) -> TableCounts;
    pub fn all_objects(&self) -> Vec<ObjectRow>; pub fn all_changesets(&self) -> Vec<ChangesetRow>; }
impl MetaStore for MemMetaStore;
pub struct MemBlobStore { .. }
impl MemBlobStore { pub fn new() -> Self; pub fn keys(&self) -> Vec<String>; pub fn raw(&self, key: &str) -> Option<Bytes>;
    pub fn corrupt(&self, key: &str) /* flips one bit */; pub fn set_down(&self, down: bool) /* all ops UNAVAILABLE */; }
impl BlobStore for MemBlobStore;

// keys.rs (SPEC 5.1)
pub fn object_key(prefix: &str, repo: RepoId, id: &ObjectId, area: Area) -> String; // <prefix>/{r|q}/<repo_id>/<letter>/<hex[0:2]>/<hex>

// repo.rs : ObjectRepo facade
#[derive(Clone, Debug)] pub struct ObjectRepoConfig { pub prefix: String, pub inline_threshold_bytes: usize /*65536*/,
    pub zstd_level: i32 /*3*/, pub max_blob_bytes: u64, pub decoded_cache_entries: usize }
#[derive(Clone, Debug)] pub enum StoredFrame { Zstd(Bytes) /* exactly as in object storage */, Raw(Bytes) /* inline data */ }
#[derive(Clone)] pub struct ObjectRepo { .. }
impl ObjectRepo {
    pub fn new(storage: Storage, cfg: ObjectRepoConfig) -> Self;          // P00 real
    pub fn storage(&self) -> &Storage;  pub fn config(&self) -> &ObjectRepoConfig;
    pub fn key(&self, id: &ObjectId, area: Area) -> String;
    pub async fn put_blob(&self, raw: Bytes) -> Result<(ObjectId, InsertOutcome)>;               // TOO_LARGE; bytes then row
    pub async fn put_encoded(&self, kind: Kind, canonical: Bytes) -> Result<(ObjectId, InsertOutcome)>; // decodes+validates known kinds; inline if <= threshold (Tree, Meta, Derived)
    pub async fn put_verified(&self, id: &ObjectId, data: Bytes) -> Result<InsertOutcome>;       // INTEGRITY on mismatch, REDACTED if blocklisted
    pub async fn stat(&self, id: &ObjectId) -> Result<Option<ObjectRow>>;                        // stale first; miss -> strong
    pub async fn stat_many(&self, ids: &[ObjectId]) -> Result<Vec<Option<ObjectRow>>>;
    pub async fn get(&self, id: &ObjectId) -> Result<Bytes>;                    // raw verified bytes; NOT_FOUND | REDACTED | INTEGRITY(+alert)
    pub async fn get_frame(&self, id: &ObjectId) -> Result<(StoredFrame, ObjectRow)>; // verified, then returned as stored
    pub async fn get_tree(&self, id: &ObjectId) -> Result<Arc<Tree>>;          // decoded LRU (immutable; survives metadata outage)
    pub async fn get_meta(&self, id: &ObjectId) -> Result<Arc<Meta>>;
    pub async fn get_changeset(&self, id: &ObjectId) -> Result<(Arc<Changeset>, ChangesetRow)>; // LRU
    pub async fn quarantine_bytes(&self, id: &ObjectId) -> Result<()>;         // copy live->quarantine, then delete live
    pub async fn restore_bytes(&self, id: &ObjectId) -> Result<()>;
    pub async fn purge_bytes(&self, id: &ObjectId) -> Result<()>;              // delete live + quarantine keys
}

// instrument.rs : 19.0 Storage Access Counters + fault decorators
#[derive(Default, Debug)] pub struct Counters { .. /* AtomicU64 fields */ }
impl Counters { pub fn snapshot(&self) -> CounterSnapshot; pub fn reset(&self);
    pub fn record_object_read(&self, kind: Kind, bytes: u64); pub fn record_object_write(&self, kind: Kind);
    pub fn record_integrity_failure(&self); }
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CounterSnapshot { pub meta_txns: u64, pub meta_rows_read: u64, pub object_paths_rows_read: u64, pub lease_calls: u64,
    pub blob_gets: u64, pub blob_puts: u64, pub blob_bytes_read: u64, pub trees_read: u64, pub blobs_read: u64,
    pub changesets_read: u64, pub metas_read: u64, pub derived_read: u64, pub trees_written: u64, pub objects_written: u64,
    pub object_bytes_read: u64, pub integrity_failures: u64 }
impl std::ops::Sub for CounterSnapshot;   // delta between snapshots
pub struct InstrumentedMeta<M: MetaStore> { .. }  impl<M: MetaStore> InstrumentedMeta<M> { pub fn new(inner: M, c: Arc<Counters>, f: Arc<FaultRegistry>) -> Self; }
pub struct InstrumentedBlob<B: BlobStore> { .. }  impl<B: BlobStore> InstrumentedBlob<B> { pub fn new(inner: B, c: Arc<Counters>, f: Arc<FaultRegistry>) -> Self; }
// Object reads are counted by ObjectRepo (inline trees count as trees_read). Leases count lease_calls and emit a scion::lock span.

// fsck.rs : SPEC 4.5 invariant oracle used by every crash test
#[derive(Clone, Debug)] pub struct FsckOptions { pub required_eager: Vec<(String, u32)>, pub verify_bytes: bool }
#[derive(Clone, Debug, PartialEq)]
pub enum Violation { MissingObject { changeset: ObjectId, object: ObjectId, role: &'static str }, DigestMismatch { id: ObjectId },
    BadEncoding { id: ObjectId }, DeltaMismatch { changeset: ObjectId }, MissingEager { changeset: ObjectId, ty: String },
    BadGraph { changeset: ObjectId }, RefTargetMissing { name: String } }
pub async fn fsck(objects: &ObjectRepo, opts: &FsckOptions) -> Result<Vec<Violation>>;  // quarantined/purged bytes are not violations

// conformance.rs (always compiled, test-support API): shared backend suites. Mem always runs them; CRDB/S3 run them when env-gated
pub async fn meta_store_suite<F, Fut>(factory: F) where F: Fn() -> Fut, Fut: Future<Output = Arc<dyn MetaStore>>;
pub async fn blob_store_suite<F, Fut>(factory: F) where F: Fn() -> Fut, Fut: Future<Output = Arc<dyn BlobStore>>;
```

### 2.4 scion-store-crdb, scion-store-s3

```rust
// scion-store-crdb
#[derive(Clone)] pub struct CrdbConfig { pub url: String, pub max_connections: usize, pub repo_name: String,
    pub tls: Option<Arc<rustls::ClientConfig>> }
pub struct CrdbMetaStore { .. }
impl CrdbMetaStore {
    pub async fn connect(cfg: CrdbConfig) -> Result<Self>;          // ensures repos row
    pub async fn migrate(&self) -> Result<()>;                       // idempotent
    pub async fn connect_fresh_for_tests(base_url: &str) -> Result<Self>; // CREATE DATABASE scion_test_<rand>; migrate
}
impl MetaStore for CrdbMetaStore;  // SERIALIZABLE; bounded 40001 retry loop; Stale => AS OF SYSTEM TIME follower_read_timestamp()
pub const MIGRATIONS: &[(&str, &str)];   // (name, sql); all SPEC 5.2 tables + audit_log + secondary indexes
pub fn test_url() -> Option<String>;     // SCION_TEST_CRDB_URL

// scion-store-s3
#[derive(Clone, Debug)] pub struct S3Config { pub endpoint: String, pub bucket: String, pub region: String,
    pub access_key: Option<String>, pub secret_key: Option<String>, pub allow_http: bool, pub path_style: bool, pub key_prefix: String }
pub struct S3BlobStore { .. }
impl S3BlobStore { pub fn new(cfg: S3Config) -> Result<Self>; }
impl BlobStore for S3BlobStore;
pub fn test_config() -> Option<S3Config>; // SCION_TEST_S3_ENDPOINT (gate), _BUCKET, _ACCESS_KEY, _SECRET_KEY, _REGION; unique key_prefix per run
```

### 2.5 scion-merge

```rust
pub const ENGINE: &str = "line-diff3-v1";
pub fn split_lines(b: &[u8]) -> Vec<&[u8]>;              // on '\n', terminators kept, final unterminated line is a line
#[derive(Clone, Debug, PartialEq, Eq)] pub enum Edit { Equal { a: usize, b: usize, len: usize }, Delete { a: usize, len: usize }, Insert { b: usize, len: usize } }
pub fn myers(a: &[&[u8]], b: &[&[u8]]) -> Vec<Edit>;      // EXACTLY docs/contracts/line-diff3-v1.md
#[derive(Clone, Debug, PartialEq, Eq)] pub struct Hunk { pub base: Range<usize>, pub new: Range<usize> }
pub fn diff_lines(a: &[&[u8]], b: &[&[u8]]) -> Vec<Hunk>;
pub fn is_binary(b: &[u8]) -> bool;                       // 0x00 within first 8000 bytes
#[derive(Clone, Debug, PartialEq, Eq)] pub enum Merge3 { Clean(Vec<u8>), Conflict(Vec<ConflictHunk>) }
pub fn merge3(base: &[u8], ours: &[u8], theirs: &[u8]) -> Merge3;
#[derive(Clone, Debug, PartialEq, Eq)] pub enum EntryDecision { Take(Option<EntryInfo>), Conflict(ConflictKind), NeedContent }
pub fn classify(b: Option<&EntryInfo>, o: Option<&EntryInfo>, t: Option<&EntryInfo>) -> EntryDecision; // 8.3 rows 1-6 (incl. symlink)
pub fn merge_mode(b: Mode, o: Mode, t: Mode) -> Mode;     // first three rows applied to modes
pub fn render_markers(base: &[u8], ours: &[u8], theirs: &[u8]) -> Vec<u8>; // "<<<<<<< ours\n" "||||||| base\n" "=======\n" ">>>>>>> theirs\n"
pub fn line_origins(parent: &[u8], child: &[u8]) -> Vec<Option<usize>>;    // child line -> parent line (blame)
#[derive(Clone, Debug, PartialEq, Eq)] pub struct TextHunk { pub old_start: u32, pub old_lines: u32, pub new_start: u32,
    pub new_lines: u32, pub lines: Vec<(u8 /* b' ' | b'-' | b'+' */, Vec<u8>)> }
pub fn unified_hunks(a: &[u8], b: &[u8], context: usize) -> Vec<TextHunk>;
```

### 2.6 scion-history

```rust
// config.rs / types.rs / hooks.rs (P00, frozen)
#[derive(Clone, Debug)] pub struct LandConfig { pub max_attempts: u32 /*32*/, pub backoff_base: Duration /*5ms*/, pub backoff_max: Duration /*500ms*/, pub rng_seed: Option<u64> }
#[derive(Clone, Debug)] pub struct HistoryConfig { pub limits: Limits, pub land: LandConfig, pub bootstrap_principal: Principal /* service:scion */ }
#[derive(Clone, Debug, PartialEq, Eq)] pub struct MetaFields { pub author: Vec<u8>, pub author_time: Timestamp,
    pub committer: Option<Vec<u8>>, pub committer_time: Option<Timestamp>, pub message: Vec<u8>, pub extensions: Extensions }
#[derive(Clone, Debug)] pub struct CreateRequest { pub parents: Vec<ObjectId>, pub delta: Vec<DeltaEntry>, pub meta: MetaFields,
    pub change_id: ChangeId, pub expected_root_tree: Option<ObjectId>,
    pub import_mode: bool /* requires WriteAuthorizer::can_import(); allows parentless; enables git_mapping */ }
#[derive(Clone, Debug, PartialEq, Eq)] pub struct Created { pub changeset_id: ObjectId, pub root_tree: ObjectId, pub meta_id: ObjectId }
#[derive(Clone, Debug)] pub enum MetaSource { New(MetaFields), Existing(ObjectId) }
#[derive(Clone, Debug)] pub struct ChangesetRecord { pub id: ObjectId, pub changeset: Arc<Changeset>, pub generation: u64,
    pub skip: Vec<ObjectId>, pub created_by: String }
#[derive(Clone, Debug)] pub struct Applied { pub root: ObjectId, pub new_trees: Vec<(RepoPath, ObjectId, Bytes /* canonical */)> }
#[derive(Clone, Debug, PartialEq, Eq)] pub struct FileChange { pub path: RepoPath, pub old: Option<EntryInfo>, pub new: Option<EntryInfo> }
#[derive(Clone, Debug)] pub struct UpdateRefRequest { pub name: RefName, pub expected_version: u64, pub new_target: ObjectId, pub allow_non_fast_forward: bool }
#[derive(Clone, Debug)] pub struct LandRequest { pub ref_name: RefName, pub head: ObjectId, pub allow_empty: bool }
#[derive(Clone, Debug, PartialEq)] pub struct LandResult { pub target: ObjectId, pub version: u64, pub landed: Vec<LandedEntry> }
pub struct EagerInput<'a> { pub id: &'a ObjectId, pub changeset: &'a Changeset, pub meta: &'a Meta,
    pub new_trees: &'a [(RepoPath, ObjectId)], pub import_mode: bool }
#[async_trait] pub trait EagerHook: Send + Sync { async fn derive_eager(&self, input: &EagerInput<'_>) -> Result<Vec<DerivedRow>>;
    fn required_eager(&self) -> Vec<(String, u32)>; }
pub struct NoEager;  // impl EagerHook: no rows
pub trait WriteAuthorizer: Send + Sync { fn can_write(&self, path: &RepoPath) -> bool; fn can_import(&self) -> bool; }
pub struct AllowAll; // impl WriteAuthorizer + LandAuthorizer + LandAuthorizerSource (tests)
pub trait LandAuthorizer: Send + Sync { fn can_update(&self, r: &RefName) -> bool; fn can_force(&self, r: &RefName) -> bool; fn can_write(&self, p: &RepoPath) -> bool; }
pub trait LandAuthorizerSource: Send + Sync { fn current(&self) -> Arc<dyn LandAuthorizer>; }  // re-read every land attempt
#[async_trait] pub trait Rebaser: Send + Sync { async fn rebase(&self, c: &ObjectId, onto: &ObjectId, who: &Principal) -> Result<ObjectId>; }
#[derive(Clone, Debug, PartialEq, Eq)] pub enum LandAttemptOutcome { Committed, Idempotent, CasMismatch, LandedConflict, Failed(ErrorCode) }
pub trait LandObserver: Send + Sync { fn attempt(&self, ref_name: &RefName, attempt: u32, outcome: &LandAttemptOutcome, elapsed: Duration); }
pub struct NoopLandObserver; // impl LandObserver
// graph_compute.rs (P00, real)
pub fn compute_commit_graph(first_parent: Option<&ChangesetRow>, parents: &[ChangesetRow], skip_lookup: impl Fn(&ObjectId, usize) -> Option<ObjectId>) -> (u64, Vec<ObjectId>);

// history.rs (struct + new: P00 real; rest by owning packages; impl blocks live in the owning module files)
pub struct History { .. }
impl History {
    pub fn new(objects: ObjectRepo, cfg: HistoryConfig, eager: Arc<dyn EagerHook>, clock: Arc<dyn Clock>) -> Self;
    pub fn objects(&self) -> &ObjectRepo; pub fn meta(&self) -> &Arc<dyn MetaStore>; pub fn config(&self) -> &HistoryConfig;
    pub fn with_land_observer(self, obs: Arc<dyn LandObserver>) -> Self;   // P00 real; default NoopLandObserver
    // P12 (tree.rs, apply.rs, diff.rs, create.rs, bootstrap.rs)
    pub async fn bootstrap(&self) -> Result<ObjectId>;   // idempotent: root changeset (empty tree) + ref main@1
    pub async fn tree(&self, id: &ObjectId) -> Result<Arc<Tree>>;
    pub async fn lookup(&self, root: &ObjectId, path: &RepoPath) -> Result<Option<TreeEntry>>;
    pub async fn changeset(&self, id: &ObjectId) -> Result<ChangesetRecord>;
    pub async fn apply_delta(&self, t1: &ObjectId, delta: &[DeltaEntry]) -> Result<Applied>;
    pub async fn file_diff(&self, a: Option<&ObjectId>, b: &ObjectId, scope: Option<&[RepoPath]>) -> Result<Vec<FileChange>>;
    pub async fn create_changeset(&self, req: CreateRequest, who: &Principal, authz: &dyn WriteAuthorizer) -> Result<Created>;
    pub async fn create_internal(&self, parents: Vec<ObjectId>, delta: Vec<DeltaEntry>, meta: MetaSource, change_id: ChangeId, who: &Principal) -> Result<Created>;
    // P13 (graph.rs)
    pub async fn is_ancestor(&self, a: &ObjectId, b: &ObjectId) -> Result<bool>;
    pub async fn is_first_parent_ancestor(&self, a: &ObjectId, b: &ObjectId) -> Result<bool>;
    pub async fn merge_bases(&self, a: &ObjectId, b: &ObjectId) -> Result<Vec<ObjectId>>;
    pub async fn nth_first_parent(&self, c: &ObjectId, n: u32) -> Result<Option<ObjectId>>;
    pub async fn nth_parent(&self, c: &ObjectId, k: u32) -> Result<Option<ObjectId>>;
    pub async fn log_page(&self, from: &ObjectId, first_parent_only: bool, limit: usize, after: Option<&[u8]>) -> Result<(Vec<ObjectId>, Option<Vec<u8>>)>;
    // P19 (rebase.rs, merge.rs, conflict.rs)
    pub async fn rebase(&self, c: &ObjectId, onto: &ObjectId, who: &Principal) -> Result<ObjectId>; // MERGE_CONFLICT | EMPTY_CHANGE
    pub async fn merge(&self, a: &ObjectId, b: &ObjectId, meta: MetaFields, change_id: ChangeId, who: &Principal, authz: &dyn WriteAuthorizer) -> Result<ObjectId>;
    // P20 (refs.rs, land.rs)
    pub async fn resolve_ref(&self, name: &RefName, at_version: Option<u64>) -> Result<RefRow>;
    pub async fn update_ref(&self, req: UpdateRefRequest, who: &Principal, authz: &dyn LandAuthorizer) -> Result<RefRow>;
    pub async fn create_ref(&self, name: &RefName, target: &ObjectId, who: &Principal, reason: &str) -> Result<RefRow>; // caller checks admin.refs
    pub async fn delete_ref(&self, name: &RefName, expected_version: u64, who: &Principal, reason: &str) -> Result<()>;
    pub async fn ref_log(&self, name: &RefName, from_version: u64, limit: usize) -> Result<Vec<RefLogRow>>;
    pub async fn land(&self, req: LandRequest, who: &Principal, authz: &dyn LandAuthorizerSource) -> Result<LandResult>;
    pub async fn land_with(&self, req: LandRequest, who: &Principal, authz: &dyn LandAuthorizerSource, rebaser: &dyn Rebaser) -> Result<LandResult>;
}
impl Rebaser for History;   // in rebase.rs (P19)
```

### 2.7 scion-derived

```rust
// types.rs (P00, frozen)
#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub enum DeriveMode { Eager, Lazy }
pub trait DerivedType: Send + Sync { fn name(&self) -> &'static str; fn version(&self) -> u32; fn mode(&self) -> DeriveMode; fn content_bearing(&self) -> bool; }
#[async_trait] pub trait DeriveCtx: Send + Sync {          // mockable (history/blame are tested through a fake ctx)
    async fn changeset(&self, id: &ObjectId) -> Result<ChangesetRecord>;
    async fn tree(&self, id: &ObjectId) -> Result<Arc<Tree>>;
    async fn meta(&self, id: &ObjectId) -> Result<Option<Arc<Meta>>>;   // None when redacted/purged
    async fn blob_or_empty(&self, id: &ObjectId) -> Result<Bytes>;     // redacted/purged content => empty (13.5)
    async fn get_object(&self, id: &ObjectId) -> Result<Bytes>;
    async fn put_object(&self, kind: Kind, canonical: Bytes) -> Result<ObjectId>;
    async fn stored(&self, key: &DerivedKey) -> Result<Option<Bytes>>;
}
#[async_trait] pub trait EagerType: DerivedType { async fn derive(&self, cx: &dyn DeriveCtx, input: &EagerInput<'_>) -> Result<Vec<DerivedRow>>; }
#[async_trait] pub trait LazyType: DerivedType {
    async fn deps(&self, cx: &dyn DeriveCtx, key: &[u8]) -> Result<Vec<Bytes>>;                         // keys of the same type needed first
    async fn derive(&self, cx: &dyn DeriveCtx, key: &[u8], deps: &[(Bytes, Bytes)] /* (key, value) */) -> Result<Bytes>;
}
#[derive(Clone, Debug)] pub struct DeriveConfig { pub lease_ttl: Duration, pub lazy_workers: usize, pub holder_id: String }
pub trait DeriveMetrics: Send + Sync { fn derive_duration(&self, ty: &str, mode: DeriveMode, d: Duration); fn backlog(&self, ty: &str, delta: i64); }

// registry.rs, engine.rs, ctx.rs, lease.rs (P14)
pub struct Registry { .. }
impl Registry { pub fn empty() -> Self; pub fn v1_default() -> Self; // descriptors commit_graph/path_index; eager history, git_mapping; lazy blame
    pub fn register_eager(&mut self, t: Arc<dyn EagerType>); pub fn register_lazy(&mut self, t: Arc<dyn LazyType>);
    pub fn register_descriptor(&mut self, name: &'static str, version: u32, mode: DeriveMode, content_bearing: bool);
    pub fn latest_version(&self, name: &str) -> Option<u32>; pub fn content_bearing_types(&self) -> Vec<String>; }
pub struct StoreDeriveCtx { .. } impl StoreDeriveCtx { pub fn new(objects: ObjectRepo) -> Self; } impl DeriveCtx for StoreDeriveCtx;
pub struct DerivedEngine { .. }
impl DerivedEngine {
    pub fn new(objects: ObjectRepo, registry: Registry, cfg: DeriveConfig, clock: Arc<dyn Clock>) -> Arc<Self>; // P00 real
    pub fn registry(&self) -> &Registry; pub fn ctx(&self) -> &dyn DeriveCtx;
    pub fn set_metrics(&self, m: Arc<dyn DeriveMetrics>);   // P00 real (stores the hook); scion-server wires Prometheus
    pub async fn get(&self, ty: &str, version: Option<u32>, key: &[u8]) -> Result<Bytes>; // generic path; lazy => iterative derive + write-back
    pub async fn get_stored(&self, ty: &str, version: Option<u32>, key: &[u8]) -> Result<Option<Bytes>>;
    pub async fn purge_content_for(&self, id: &ObjectId) -> Result<u64>;  // delete content-bearing records keyed by source id
}
impl EagerHook for DerivedEngine;
// history_type.rs, path_index.rs, git_mapping.rs (P21)
pub mod history { pub const NAME: &str = "history"; pub const VERSION: u32 = 1; pub struct HistoryType; // EagerType
    pub async fn root_node(cx: &dyn DeriveCtx, cs: &ObjectId) -> Result<ObjectId>;
    pub async fn node_for_path(cx: &dyn DeriveCtx, cs: &ObjectId, path: &RepoPath) -> Result<Option<(ObjectId, HistoryNode)>>;
    pub async fn get_node(cx: &dyn DeriveCtx, id: &ObjectId) -> Result<HistoryNode>; }
pub mod path_index { pub const NAME: &str = "path_index"; pub const VERSION: u32 = 1; }      // rows computed by History::create_changeset
pub mod commit_graph { pub const NAME: &str = "commit_graph"; pub const VERSION: u32 = 1; }  // stored on the changesets row
pub mod git_mapping { pub const NAME: &str = "git_mapping"; pub const VERSION: u32 = 1; pub struct GitMappingType; // EagerType, import_mode + Meta ext 2 only
    pub async fn changeset_for_git(e: &DerivedEngine, sha_hex: &str) -> Result<Option<ObjectId>>;
    pub async fn git_for_changeset(e: &DerivedEngine, cs: &ObjectId) -> Result<Option<String>>; }
// blame.rs, log.rs, versions.rs (P24)
pub mod blame { pub const NAME: &str = "blame"; pub struct BlameType { .. } impl BlameType { pub fn new() -> Self; pub fn with_version(v: u32) -> Self; }
    #[derive(Clone, Debug, PartialEq, Eq)] pub struct BlameRange { pub start_line: u32, pub line_count: u32, pub origin_changeset: ObjectId, pub origin_start_line: u32 }
    pub fn encode_ranges(r: &[BlameRange]) -> Bytes; pub fn decode_ranges(b: &[u8]) -> Result<Vec<BlameRange>>;
    pub async fn blame(e: &DerivedEngine, cs: &ObjectId, path: &RepoPath, version: Option<u32>) -> Result<Vec<BlameRange>>; }
pub mod log { pub struct PathLogPage { pub changesets: Vec<ObjectId>, pub next_page: Option<Vec<u8>> }
    pub async fn log_path(e: &DerivedEngine, from: &ObjectId, path: &RepoPath, first_parent_only: bool, limit: usize, page: Option<&[u8]>) -> Result<PathLogPage>; }
pub mod versions { pub async fn all_versions(e: &DerivedEngine, at: &ObjectId, path: &RepoPath) -> Result<Vec<ObjectId>>; } // distinct blob ids, first-seen order
```

### 2.8 scion-redaction

```rust
#[derive(Clone, Debug)] pub struct RedactionConfig { pub propagation_bound: Duration /*1s*/, pub default_quarantine: Duration /*7d*/, pub purge_interval: Duration }
#[derive(Clone, Debug, PartialEq)] pub struct RedactOutcome { pub epoch: u64, pub effective_by: SystemTime }
pub struct Redactor { .. }
impl Redactor {
    pub fn new(objects: ObjectRepo, derived: Arc<DerivedEngine>, clock: Arc<dyn Clock>, cfg: RedactionConfig) -> Arc<Self>; // P00 real
    pub async fn redact(&self, ids: &[ObjectId], reason: &str, actor: &Principal, purge_after: Option<Duration>) -> Result<RedactOutcome>; // Blob/Meta only
    pub async fn unredact(&self, ids: &[ObjectId], actor: &Principal) -> Result<u64>;
    pub async fn purge_now(&self, ids: &[ObjectId], actor: &Principal, reason: &str) -> Result<()>;
    pub async fn run_due_purges(&self) -> Result<usize>;
    pub async fn move_pending_bytes(&self) -> Result<usize>;      // finish quarantine copies (idempotent)
    pub async fn list(&self, since_epoch: u64) -> Result<(Vec<RedactionRow>, u64)>;
    pub fn spawn_background(self: &Arc<Self>) -> tokio::task::JoinHandle<()>; // mover + purge scheduler
}
```

### 2.9 scion-policy

```rust
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PolicyError { Toml(String), UnknownKey(String), UnsupportedVersion(i64), MissingRootRule, GroupCycle(Vec<String>),
    UnknownGroup(String), BadPrincipal(String), BadPrefix(String), BadRefPattern(String), Io(String) }
#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub enum AdminPerm { Redact, Purge, Refs }
pub struct Policy { .. }
impl Policy {
    pub fn parse(toml: &str) -> Result<Self, PolicyError>;
    pub fn epoch(&self) -> u64;                                    // BLAKE3(normalized policy)[0..8] big-endian
    pub fn can_read(&self, p: &Principal, path: &RepoPath) -> bool;
    pub fn can_write(&self, p: &Principal, path: &RepoPath) -> bool;
    pub fn read_governed_by_root(&self, path: &RepoPath) -> bool;  // longest matching read rule is "/"
    pub fn can_update_ref(&self, p: &Principal, r: &RefName) -> bool; pub fn can_force_ref(&self, p: &Principal, r: &RefName) -> bool;
    pub fn can_admin(&self, p: &Principal, a: AdminPerm) -> bool;
    pub fn groups_of(&self, p: &Principal) -> Vec<String>;
    pub fn allow_all_for_tests() -> Self;
}
impl From<PolicyError> for ScionError;
pub struct PolicyHandle { .. }   // arc-swap + notify watcher (+ 1s polling fallback)
impl PolicyHandle {
    pub fn load(path: &Path) -> Result<Arc<Self>, PolicyError>;     // invalid/missing => Err (startup fails)
    pub fn from_policy(p: Policy) -> Arc<Self>;
    pub fn current(&self) -> Arc<Policy>; pub fn epoch(&self) -> u64;
    pub fn reload_now(&self) -> Result<bool /* changed */, PolicyError>;  // invalid keeps last good, records last_error
    pub fn last_error(&self) -> Option<PolicyError>;
    pub fn spawn_watcher(self: &Arc<Self>) -> tokio::task::JoinHandle<()>;
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64>;
}
```

### 2.10 scion-auth

```rust
#[derive(Clone, Debug, PartialEq)] pub struct Claims { pub sub: Principal, pub gid: Option<u64>, pub iat: SystemTime, pub exp: SystemTime, pub kid: String }
#[derive(Clone, Debug, PartialEq, Eq)] pub struct PublicKeyInfo { pub kid: String, pub ed25519_public: [u8; 32] }
#[derive(Clone)] pub struct Keyring { .. }      // first key signs; all verify
impl Keyring { pub fn from_files(paths: &[PathBuf]) -> Result<Self>;    // PKCS#8 PEM Ed25519
    pub fn generate_for_tests(n: usize) -> Self; pub fn write_pem_files(&self, dir: &Path) -> Result<Vec<PathBuf>>;
    pub fn issue(&self, sub: &Principal, ttl: Duration, now: SystemTime) -> Result<(String, Claims)>;
    pub fn sign(&self, c: &Claims) -> Result<String>; pub fn public_keys(&self) -> Vec<PublicKeyInfo>;
    pub fn verifier(&self) -> Verifier; }
#[derive(Clone)] pub struct Verifier { .. }
impl Verifier { pub fn new(keys: Vec<PublicKeyInfo>) -> Self; pub fn verify(&self, token: &str, now: SystemTime) -> Result<Claims>; } // UNAUTHENTICATED
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool;   // the single constant-time primitive (subtle)
#[cfg(feature = "test-hooks")] pub fn ct_eq_calls() -> u64;
pub struct Secret(String);                  // Debug/Display print "***"; impl Secret { pub fn new(s: String) -> Self; pub fn expose(&self) -> &str }
pub struct ServicePrincipals { .. }
impl ServicePrincipals { pub fn new(entries: Vec<(String, String /* sha256 lowercase hex */)>) -> Result<Self>;
    pub fn verify(&self, name: &str, secret: &[u8]) -> bool; }   // always hashes + ct_eq (unknown names compare against a dummy digest)
#[derive(Clone, Debug, PartialEq, Eq)] pub struct GitHubUser { pub login: String, pub id: u64 }
#[derive(Clone)] pub struct GitHubClient { .. }
impl GitHubClient { pub fn new(api_url: &str) -> Self; pub async fn user(&self, access_token: &str) -> Result<GitHubUser>; } // 401 => UNAUTHENTICATED; transport/5xx => UNAVAILABLE
pub mod device_flow { #[derive(Clone, Debug)] pub struct DeviceCode { pub device_code: String, pub user_code: String, pub verification_uri: String, pub interval: Duration, pub expires_in: Duration }
    pub async fn start(oauth_base: &str, client_id: &str) -> Result<DeviceCode>;
    pub async fn poll(oauth_base: &str, client_id: &str, dc: &DeviceCode) -> Result<String>; } // handles authorization_pending / slow_down
```

### 2.11 scion-config

```rust
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ConfigError { UnknownKey { key: String /* full dotted path */ }, UnresolvedVar { name: String, key: String },
    Invalid { key: String, msg: String }, Parse(String), Io(String) }
pub trait Env: Send + Sync { fn var(&self, name: &str) -> Option<String>; }
pub struct ProcessEnv; impl Env for ProcessEnv;  pub struct MapEnv(pub HashMap<String, String>); impl Env for MapEnv;
// ServerConfig: sections exactly as SPEC 6.1 plus implementation-defined keys marked (+)
pub struct ServerConfig { pub server: ServerSection, pub metadata: MetadataSection, pub objects: ObjectsSection, pub auth: AuthSection,
    pub policy: PolicySection, pub limits: Limits, pub land: LandSection, pub derive: DeriveSection, pub redaction: RedactionSection,
    pub observability: ObservabilitySection, pub rate_limit: RateLimitSection /* (+) */ }
pub struct ServerSection { pub listen: SocketAddr, pub tls_cert: PathBuf, pub tls_key: PathBuf, pub repo: String }
pub struct MetadataSection { pub url: String, pub max_connections: usize /*64*/ }
pub struct ObjectsSection { pub endpoint: String, pub bucket: String, pub prefix: String, pub inline_threshold_bytes: usize, pub zstd_level: i32,
    pub region: String /* (+) "us-east-1" */, pub path_style: bool /* (+) true */ }
pub struct AuthSection { pub github_client_id: String, pub github_api_url: String, pub github_oauth_base_url: String /* (+) https://github.com */,
    pub token_ttl: Duration /*12h*/, pub signing_keys: Vec<PathBuf>, pub service_principals: Vec<ServicePrincipalConfig> }
pub struct ServicePrincipalConfig { pub name: String, pub secret_sha256: String }
pub struct PolicySection { pub path: PathBuf, pub reload: bool }
pub struct LandSection { pub max_attempts: u32, pub backoff_base_ms: u64, pub backoff_max_ms: u64 }
pub struct DeriveSection { pub lease_ttl_ms: u64, pub lazy_workers: usize }
pub struct RedactionSection { pub propagation_bound_ms: u64, pub default_quarantine: Duration }
pub struct ObservabilitySection { pub log_format: String /* json|text */, pub metrics_listen: SocketAddr }
pub struct RateLimitSection { pub per_principal_rps: u32 /*500*/, pub burst: u32 /*1000*/, pub max_inflight: usize /*4096*/ }
pub struct DaemonConfig { pub socket: PathBuf, pub upstream: String, pub cache_dir: PathBuf, pub max_cache_bytes: u64, pub low_watermark_ratio: f64,
    pub verify_on_read: bool, pub ref_cache_ttl_ms: u64, pub access_cache_ttl_ms: u64, pub redaction_poll_interval_ms: u64,
    pub redaction_max_staleness_ms: u64, pub upstream_max_concurrency: usize, pub upstream_queue: usize /* (+) 4096 */,
    pub upstream_ca: Option<PathBuf> /* (+) */ }
pub struct ClientConfig { pub endpoint: Option<String> }
pub struct WcConfig { pub endpoint: Option<String>, pub tracking_ref: Option<String> }   // <root>/.scion/config.toml
pub fn parse_server(text: &str, env: &dyn Env) -> Result<ServerConfig, ConfigError>;  pub fn load_server(p: &Path, env: &dyn Env) -> Result<ServerConfig, ConfigError>;
pub fn parse_daemon(text: &str, env: &dyn Env) -> Result<DaemonConfig, ConfigError>;  pub fn load_daemon(p: &Path, env: &dyn Env) -> Result<DaemonConfig, ConfigError>;
pub fn parse_client(text: &str, env: &dyn Env) -> Result<ClientConfig, ConfigError>;  pub fn parse_wc(text: &str, env: &dyn Env) -> Result<WcConfig, ConfigError>;
pub fn resolve_endpoint(flag: Option<&str>, env: &dyn Env, wc: Option<&WcConfig>, user: Option<&ClientConfig>) -> Option<String>; // flag > SCION_ENDPOINT > wc > user
impl ServerConfig { pub fn for_tests(dir: &Path) -> Self; }  impl DaemonConfig { pub fn for_tests(dir: &Path, upstream: &str) -> Self; }
```

### 2.12 scion-obs

```rust
#[derive(Copy, Clone, Debug)] pub enum LogFormat { Json, Text }
pub fn init_logging(format: LogFormat) -> LogGuard;      // tracing-subscriber; JSON one event per line
pub struct LogGuard;
#[derive(Clone, Debug, Default)]
pub struct RpcLogEvent { pub request_id: String, pub rpc: String, pub tier: &'static str, pub principal: Option<String>, pub repo: String,
    pub status: String, pub duration_ms: u64, pub changeset_id: Option<String>, pub ref_name: Option<String> /* logged as "ref" */,
    pub ref_version: Option<u64>, pub land_attempt: Option<u32> }
impl RpcLogEvent { pub fn emit(&self); }                 // target "scion::rpc", level INFO
pub fn path_hash(path: &[u8]) -> String;                 // 16 hex of BLAKE3; the ONLY way paths may appear in logs
pub struct Metrics { pub registry: prometheus::Registry, pub rpc_duration: HistogramVec, pub cache_requests: IntCounterVec,
    pub cache_bytes: IntGaugeVec, pub land_attempts: IntCounterVec, pub ref_cas_failures: IntCounterVec, pub land_duration: Histogram,
    pub derive_duration: HistogramVec, pub derive_backlog: IntGaugeVec, pub redaction_epoch: IntGaugeVec, pub redaction_lag: GaugeVec,
    pub policy_epoch_info: IntGaugeVec, pub object_bytes_served: IntCounterVec, pub upstream_requests: IntCounterVec,
    pub integrity_failures: IntCounterVec /* (+) scion_integrity_failures_total{tier}: the 16.1 class 7 alert */ }
impl Metrics { pub fn new() -> Arc<Self>; pub fn render(&self) -> String; pub const NAMES: &'static [&'static str]; }
pub trait Readiness: Send + Sync { fn check(&self) -> Result<(), String>; }
pub fn health_router(metrics: Arc<Metrics>, ready: Arc<dyn Readiness>) -> axum::Router;   // /metrics /healthz /readyz
pub async fn serve_admin(addr: SocketAddr, router: axum::Router) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)>;
pub mod capture { pub struct LogCapture { .. }
    impl LogCapture { pub fn install() -> Self;           // process-global, idempotent; JSON layer into memory
        pub fn events(&self) -> Vec<serde_json::Value>; pub fn raw(&self) -> String;
        pub fn find_needles(&self, needles: &[&[u8]]) -> Vec<String>; pub fn clear(&self); } }
```

### 2.13 RPC surface: `proto/scion/v1/scion.proto` (P00) and scion-proto (Rust)

The proto is the frozen wire contract. Protobuf is transport only: ids travel as 34 raw bytes, and changesets as their canonical encoding. Field numbers 1-15 are used densely, and 100+ is reserved per message for additive evolution. Required fields are marked `// required`. If one is missing, the server returns INVALID_ARGUMENT with subcode missing_field.

```proto
syntax = "proto3";
package scion.v1;

service ObjectService {            // served by every tier (7.3)
  rpc GetServerInfo(GetServerInfoRequest) returns (ServerInfo);
  rpc GetObjects(GetObjectsRequest) returns (stream ObjectChunk);
  rpc HasObjects(HasObjectsRequest) returns (HasObjectsResponse);
  rpc ResolveRef(ResolveRefRequest) returns (ResolveRefResponse);
}
service QueryService {             // server; forwarded by daemon (7.4)
  rpc GetChangesets(GetChangesetsRequest) returns (GetChangesetsResponse);
  rpc ReadPath(ReadPathRequest) returns (stream ReadPathChunk);
  rpc ListTree(ListTreeRequest) returns (ListTreeResponse);
  rpc Log(LogRequest) returns (LogResponse);
  rpc Blame(BlameRequest) returns (BlameResponse);
  rpc Diff(DiffRequest) returns (stream DiffItem);
  rpc IsAncestor(IsAncestorRequest) returns (IsAncestorResponse);
  rpc MergeBase(MergeBaseRequest) returns (MergeBaseResponse);
  rpc Resolve(ResolveRequest) returns (ResolveResponse);
  rpc GetDerived(GetDerivedRequest) returns (GetDerivedResponse);   // additive: generic derived-data path (9.1, 19.6)
}
service WriteService {             // 7.5
  rpc UploadObjects(stream UploadItem) returns (UploadObjectsResponse);
  rpc CreateChangeset(CreateChangesetRequest) returns (CreateChangesetResponse);
  rpc Rebase(RebaseRequest) returns (RebaseResponse);
  rpc Merge(MergeRequest) returns (MergeResponse);
  rpc Land(LandRequest) returns (LandResponse);
}
service RefService {               // 7.6
  rpc UpdateRef(UpdateRefRequest) returns (UpdateRefResponse);
  rpc CreateRef(CreateRefRequest) returns (CreateRefResponse);
  rpc DeleteRef(DeleteRefRequest) returns (DeleteRefResponse);
  rpc GetRefLog(GetRefLogRequest) returns (GetRefLogResponse);
}
service AuthService {              // 7.7
  rpc Login(LoginRequest) returns (LoginResponse);
  rpc WhoAmI(WhoAmIRequest) returns (WhoAmIResponse);
  rpc CheckAccess(CheckAccessRequest) returns (CheckAccessResponse);
}
service AdminService {             // 7.7
  rpc Redact(RedactRequest) returns (RedactResponse);
  rpc Unredact(UnredactRequest) returns (UnredactResponse);
  rpc PurgeNow(PurgeNowRequest) returns (PurgeNowResponse);
  rpc ListRedactions(ListRedactionsRequest) returns (ListRedactionsResponse);
}

// ---- common ----
message EntryInfo { uint32 mode = 1; bytes id = 2; uint64 size = 3; }
message TreeEntryMsg { bytes name = 1; EntryInfo entry = 2; }
message Timestamp { int64 seconds = 1; int64 tz_offset_minutes = 2; }
message MetaFields { bytes author = 1; Timestamp author_time = 2; optional bytes committer = 3; Timestamp committer_time = 4;
  bytes message = 5; repeated Extension extensions = 6; }
message Extension { uint64 tag = 1; bytes value = 2; }
message DeltaEntryMsg { bytes path = 1; uint32 op = 2 /* 1 Put 2 Delete */; uint32 mode = 3; bytes id = 4; uint64 size = 5; }
message LandedEntry { bytes change_id = 1; bytes source = 2; bytes result = 3; }
enum ItemStatus { ITEM_STATUS_UNSPECIFIED = 0; OK = 1; NOT_FOUND = 2; DENIED = 3; REDACTED = 4; }
enum Encoding { ENCODING_UNSPECIFIED = 0; RAW = 1; ZSTD = 2; }
enum AccessClass { ACCESS_CLASS_UNSPECIFIED = 0; ROOT = 1; RESTRICTED = 2; }

// ---- errors (7.2): carried as google.rpc.Status-style binary details: Status.details = ErrorDetail encoding ----
message ErrorDetail { string code = 1; bool retryable = 2; string message = 3; ErrorPayload payload = 4; }
message ErrorPayload { oneof kind {
  string subcode = 1; LimitPayload limit = 2; ConflictReport merge_conflict = 3; RefMismatch ref_mismatch = 4;
  AlreadyLandedPayload already_landed = 5; Candidates candidates = 6; } }
message LimitPayload { string name = 1; uint64 limit = 2; uint64 actual = 3; }
message ConflictHunk { uint32 base_start = 1; uint32 base_end = 2; uint32 ours_start = 3; uint32 ours_end = 4; uint32 theirs_start = 5; uint32 theirs_end = 6; }
message ConflictMsg { bytes path = 1; string kind = 2 /* add_add|modify_delete|symlink|binary|redacted|file_directory|content */;
  EntryInfo base = 3; EntryInfo ours = 4; EntryInfo theirs = 5; repeated ConflictHunk hunks = 6; }
message CleanEntry { bytes path = 1; EntryInfo entry = 2 /* absent = deleted */; }
message ConflictReport { repeated CleanEntry clean_entries = 1; repeated ConflictMsg conflicts = 2; }
message RefMismatch { uint64 current_version = 1; bytes current_target = 2; }
message AlreadyLandedPayload { bytes source = 1; bytes result = 2; uint64 version = 3; }
message Candidates { repeated bytes ids = 1; }

// ---- object ----
message GetServerInfoRequest {}
message PublicKey { string kid = 1; bytes ed25519 = 2; }
message LimitsMsg { uint64 max_blob_bytes = 1; uint64 max_delta_entries = 2; uint64 max_parents = 3; uint64 max_message_bytes = 4; uint64 merge_max_text_bytes = 5; }
message ServerInfo { repeated string protocol_versions = 1; string repo = 2; uint64 policy_epoch = 3; uint64 redaction_epoch = 4;
  repeated PublicKey token_public_keys = 5; LimitsMsg limits = 6;
  string github_client_id = 7; string github_oauth_base_url = 8; }   // 7,8: needed by 12.2 device flow
message GetObjectsRequest { repeated bytes ids = 1; optional uint64 offset = 2; optional uint64 length = 3; }
message ObjectChunk { bytes id = 1; ItemStatus status = 2; Encoding encoding = 3; bytes data = 4; uint64 total_size = 5;
  AccessClass access_class = 6; bool final = 7; uint64 offset = 8; }   // range reads => RAW; multiple chunks per id allowed; final on last
message HasObjectsRequest { repeated bytes ids = 1; }
message HasObjectsResponse { repeated bool present = 1; }
message ResolveRefRequest { string name = 1; optional uint64 at_version = 2; }
message ResolveRefResponse { bytes target = 1; uint64 version = 2; }

// ---- query ----
message GetChangesetsRequest { repeated bytes ids = 1; }
message FilteredChangeset { repeated bytes parents = 1; bytes root_tree = 2; bytes meta = 3; bytes change_id = 4;
  repeated DeltaEntryMsg delta = 5; bool filtered = 6; }
message ChangesetItem { bytes id = 1; ItemStatus status = 2; oneof view { bytes encoded = 3; FilteredChangeset filtered_view = 4; }
  uint64 generation = 5; }
message GetChangesetsResponse { repeated ChangesetItem items = 1; }
message ReadPathRequest { bytes changeset = 1; bytes path = 2; }
message ReadPathChunk { EntryInfo entry = 1 /* first chunk */; bytes data = 2; bool final = 3; }
message ListTreeRequest { bytes changeset = 1; bytes path = 2; }
message ListTreeResponse { repeated TreeEntryMsg entries = 1; bytes tree_id = 2; }
message LogRequest { bytes from = 1; optional bytes path = 2; bool first_parent_only = 3; uint32 limit = 4; bytes page_token = 5; }
message LogEntry { bytes id = 1; ChangesetItem changeset = 2; MetaView meta = 3; }
message MetaView { bool redacted = 1; bytes author = 2; Timestamp author_time = 3; optional bytes committer = 4; Timestamp committer_time = 5;
  bytes creator = 6; bytes message = 7; bytes meta_id = 8; }   // redacted => only meta_id set; clients render "<redacted>"
message LogResponse { repeated LogEntry entries = 1; bytes next_page_token = 2; }
message BlameRequest { bytes changeset = 1; bytes path = 2; }
message BlameRangeMsg { uint32 start_line = 1; uint32 line_count = 2; bytes origin_changeset = 3; uint32 origin_start_line = 4; }
message BlameResponse { repeated BlameRangeMsg ranges = 1; }
message DiffRequest { bytes a = 1; bytes b = 2; repeated bytes paths = 3; bool include_content_hunks = 4; }
message TextHunkMsg { uint32 old_start = 1; uint32 old_lines = 2; uint32 new_start = 3; uint32 new_lines = 4; repeated bytes lines = 5 /* first byte ' ' '-' '+' */; }
message DiffItem { bytes path = 1; EntryInfo old_entry = 2; EntryInfo new_entry = 3; repeated TextHunkMsg hunks = 4; bool binary = 5; }
message IsAncestorRequest { bytes a = 1; bytes b = 2; }   message IsAncestorResponse { bool result = 1; }
message MergeBaseRequest { bytes a = 1; bytes b = 2; }    message MergeBaseResponse { repeated bytes bases = 1; }
message ResolveRequest { string revision = 1; }           message ResolveResponse { bytes changeset = 1; }
message GetDerivedRequest { string type = 1; optional uint32 version = 2; bytes key = 3; }
message GetDerivedResponse { bytes value = 1; uint32 version = 2; }

// ---- write ----
message UploadItem { bytes id = 1; uint32 kind = 2; bytes data = 3 /* raw content */; }
message UploadResult { bytes id = 1; ItemStatus status = 2; string error_code = 3; string message = 4; }
message UploadObjectsResponse { repeated UploadResult results = 1; }
message CreateChangesetRequest { repeated bytes parents = 1; repeated DeltaEntryMsg delta = 2; MetaFields meta_fields = 3 /* required */;
  bytes change_id = 4 /* required, 16 bytes */; optional bytes expected_root_tree = 5;
  bool import_mode = 6; }   // implementation-defined: requires admin.refs + write on "/"; allows parentless; records git_mapping
message CreateChangesetResponse { bytes changeset_id = 1; bytes root_tree = 2; bytes meta_id = 3; }
message RebaseRequest { bytes changeset = 1; bytes onto = 2; }   message RebaseResponse { bytes changeset_id = 1; }
message MergeRequest { bytes a = 1; bytes b = 2; MetaFields meta_fields = 3; bytes change_id = 4; }  message MergeResponse { bytes changeset_id = 1; }
message LandRequest { string ref = 1; bytes head = 2; bool allow_empty = 3; }
message LandResponse { bytes target = 1; uint64 version = 2; repeated LandedEntry landed = 3; }

// ---- refs ----
message UpdateRefRequest { string name = 1; uint64 expected_version = 2; bytes new_target = 3; bool allow_non_fast_forward = 4; }
message UpdateRefResponse { uint64 version = 1; }
message CreateRefRequest { string name = 1; bytes target = 2; string reason = 3; }  message CreateRefResponse { uint64 version = 1; }
message DeleteRefRequest { string name = 1; uint64 expected_version = 2; string reason = 3; } message DeleteRefResponse {}
message GetRefLogRequest { string name = 1; uint64 from_version = 2; uint32 limit = 3; }
message RefLogEntryMsg { uint64 version = 1; bytes old_target = 2; bytes new_target = 3; string principal = 4; int64 at_unix_ms = 5; repeated LandedEntry landed = 6; }
message GetRefLogResponse { repeated RefLogEntryMsg entries = 1; }

// ---- auth / admin ----
message LoginRequest { oneof credential { string github_access_token = 1; ServiceCredential service = 2; } }
message ServiceCredential { string service_name = 1; string service_secret = 2; }
message LoginResponse { string token = 1; string principal = 2; int64 expires_at_unix = 3; }
message WhoAmIRequest {}  message WhoAmIResponse { string principal = 1; repeated string groups = 2; int64 expires_at_unix = 3; }
message CheckAccessRequest { string subject_token = 1; repeated bytes object_ids = 2; }
message CheckAccessResponse { repeated bool allowed = 1; repeated AccessClass access_class = 2; }
message RedactRequest { repeated bytes object_ids = 1; string reason = 2; optional uint64 purge_after_seconds = 3; }
message RedactResponse { uint64 epoch = 1; int64 effective_by_unix_ms = 2; }
message UnredactRequest { repeated bytes object_ids = 1; }   message UnredactResponse { uint64 epoch = 1; }
message PurgeNowRequest { repeated bytes object_ids = 1; string reason = 2; }  message PurgeNowResponse {}
message ListRedactionsRequest { uint64 since_epoch = 1; }
message RedactionEntry { bytes object_id = 1; string state = 2; uint64 epoch = 3; string reason = 4; }
message ListRedactionsResponse { repeated RedactionEntry entries = 1; uint64 current_epoch = 2; }
```

Rust surface of `scion-proto` (P00 writes `build.rs` + `v1` module; P08 implements the rest):

```rust
pub mod v1 { tonic::include_proto!("scion.v1"); pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("scion_descriptor"); }
pub fn tonic_code(c: GrpcCode) -> tonic::Code;
pub fn status_from_error(e: &ScionError) -> tonic::Status;     // code per 7.2 + ErrorDetail in details
pub fn error_from_status(s: &tonic::Status) -> ScionError;     // inverse; unknown => UNAVAILABLE
pub mod convert { /* From/TryFrom between v1::* and scion-core/scion-object/scion-store types; TryFrom errors => INVALID_ARGUMENT/missing_field|bad_id_text */ }
pub const TRAILER_POLICY_EPOCH: &str = "scion-policy-epoch"; pub const TRAILER_REDACTION_EPOCH: &str = "scion-redaction-epoch"; pub const TRAILER_TIER: &str = "scion-tier";
#[derive(Copy, Clone, Debug, PartialEq, Eq)] pub struct Epochs { pub policy: u64, pub redaction: u64, pub tier: Tier }
pub trait EpochSource: Send + Sync + 'static { fn epochs(&self) -> Epochs; }
pub fn read_epochs(h: &http::HeaderMap) -> Option<Epochs>;  pub fn write_epochs(h: &mut http::HeaderMap, e: &Epochs);
#[derive(Clone)] pub struct EpochTrailerLayer { .. }  // tower Layer on http bodies: appends the 3 trailers to the gRPC trailers frame of EVERY
impl EpochTrailerLayer { pub fn new(src: Arc<dyn EpochSource>) -> Self; }   // response: unary, streaming, error and trailers-only
#[derive(Clone, Debug)] pub enum Endpoint { Unix(PathBuf), Https(String) }  impl Endpoint { pub fn parse(s: &str) -> Result<Self>; }
#[derive(Clone, Debug, Default)] pub struct TlsOptions { pub ca_pem: Option<Vec<u8>>, pub domain: Option<String> }
pub type Transport = /* P00 chooses: InterceptedService<capture-layered Channel, BearerInterceptor> */;
#[derive(Clone)] pub struct ScionClient { .. }
impl ScionClient {
    pub async fn connect(ep: &Endpoint, tls: Option<TlsOptions>, token: Option<String>) -> Result<Self>;
    pub fn with_token(&self, token: Option<String>) -> Self;
    pub fn objects(&self) -> v1::object_service_client::ObjectServiceClient<Transport>;
    pub fn query(&self) -> v1::query_service_client::QueryServiceClient<Transport>;
    pub fn write(&self) -> v1::write_service_client::WriteServiceClient<Transport>;
    pub fn refs(&self) -> v1::ref_service_client::RefServiceClient<Transport>;
    pub fn auth(&self) -> v1::auth_service_client::AuthServiceClient<Transport>;
    pub fn admin(&self) -> v1::admin_service_client::AdminServiceClient<Transport>;
    pub fn last_epochs(&self) -> Option<Epochs>;   // captured from response trailers by a client layer
    pub fn calls(&self) -> u64; pub fn reset_calls(&self);
    pub fn channel(&self) -> tonic::transport::Channel;
}
pub mod raw { pub struct RawCodec; /* tonic::codec::Codec with Encode = Decode = bytes::Bytes; used for verbatim forwarding */ }
```

### 2.14 scion-server

```rust
pub struct ServerDeps { pub storage: Storage, pub config: ServerConfig, pub policy: Arc<PolicyHandle>, pub keyring: Keyring,
    pub github: GitHubClient, pub clock: Arc<dyn Clock>, pub metrics: Arc<Metrics>,
    pub registry: Option<Registry> /* None = Registry::v1_default() */ }
pub struct AppContext { pub storage: Storage, pub objects: ObjectRepo, pub history: Arc<History>, pub derived: Arc<DerivedEngine>,
    pub redactor: Arc<Redactor>, pub redactions: Arc<RedactionSet>, pub policy: Arc<PolicyHandle>, pub keyring: Keyring,
    pub verifier: Verifier, pub services: ServicePrincipals, pub github: GitHubClient, pub clock: Arc<dyn Clock>,
    pub metrics: Arc<Metrics>, pub config: ServerConfig, pub authz: authz::ObjectAuthorizer }
impl EpochSource for AppContext;   // policy epoch + redaction epoch, tier = Server
pub struct ServerApp { .. }
impl ServerApp {
    pub async fn build(deps: ServerDeps) -> Result<Self>;    // History::bootstrap, wiring, background tasks, policy watcher
    pub fn context(&self) -> Arc<AppContext>;
    pub fn router(&self) -> tonic::transport::server::Router; // grpc.rs services + all layers
    pub async fn serve(self, listener: tokio::net::TcpListener, tls: Option<(Vec<u8> /* cert pem */, Vec<u8> /* key pem */)>) -> Result<ServerHandle>;
}
pub struct ServerHandle { pub addr: SocketAddr, pub admin_addr: Option<SocketAddr> }  impl ServerHandle { pub async fn shutdown(self); pub fn abort(&self); }
pub struct RedactionSet { .. }    // redaction_set.rs (P23)
impl RedactionSet { pub fn new(meta: Arc<dyn MetaStore>) -> Arc<Self>; pub fn contains(&self, id: &ObjectId) -> bool; pub fn epoch(&self) -> u64;
    pub async fn refresh(&self) -> Result<u64>; pub fn observe(self: &Arc<Self>, epoch: u64); pub fn last_refresh(&self) -> Instant;
    pub fn spawn_refresher(self: &Arc<Self>, bound: Duration) -> tokio::task::JoinHandle<()>; }
#[derive(Clone, Debug)] pub struct CallerCtx { pub principal: Principal, pub claims: Claims, pub request_id: String }
pub mod context { pub fn caller<T>(req: &tonic::Request<T>) -> Result<CallerCtx, tonic::Status>; }  // P23; set by the interceptor
pub mod authz {   // mod.rs P00; object.rs P25; policy_adapters.rs P26
    #[derive(Copy, Clone, Debug, PartialEq, Eq)] pub enum AccessClass { Root, Restricted }
    #[derive(Copy, Clone, Debug, PartialEq, Eq)] pub enum ReadDecision { Allow(AccessClass), Deny, Redacted }
    pub struct ObjectAuthorizer { .. }
    impl ObjectAuthorizer { pub fn new(storage: Storage, policy: Arc<PolicyHandle>, redactions: Arc<RedactionSet>) -> Self;  // P00 real
        pub async fn can_read(&self, p: &Principal, id: &ObjectId) -> Result<ReadDecision>; }    // 18.6; cache (principal,id,policy_epoch)
    pub struct PolicyWriteAuthz { .. } impl PolicyWriteAuthz { pub fn new(policy: Arc<Policy>, who: Principal) -> Self; } // impl WriteAuthorizer
    pub struct PolicyLandAuthzSource { .. } impl PolicyLandAuthzSource { pub fn new(handle: Arc<PolicyHandle>, who: Principal) -> Self; } // impl LandAuthorizerSource
}
// grpc.rs (P00, frozen): impl of the 6 generated tonic server traits; each method is one line:
//   handlers::<file>::<rpc_snake>(&self.ctx, request).await
// handlers/<file>.rs: pub async fn <rpc_snake>(ctx: &Arc<AppContext>, req: tonic::Request<Req>) -> Result<tonic::Response<Resp>, tonic::Status>
//   server_info.rs: get_server_info                  auth.rs: login, who_am_i                       (P23)
//   object.rs: get_objects, has_objects, resolve_ref  upload.rs: upload_objects  access.rs: check_access
//   admin.rs: redact, unredact, purge_now, list_redactions, + pub fn spawn_background(ctx: &Arc<AppContext>) (no-op stub)   (P25)
//   write.rs: create_changeset, rebase, merge, land   refs.rs: update_ref, create_ref, delete_ref, get_ref_log           (P26)
//   query.rs: get_changesets, read_path, list_tree, diff, is_ancestor, merge_base, resolve   derived.rs: get_derived      (P27)
//   history_query.rs: log, blame                                                                                           (P24)
#[cfg(feature = "testing")] pub mod testing {   // P23
    #[derive(Clone)] pub struct ServerTestOpts { pub acl_toml: String, pub storage: Option<Storage>, pub mem: MemOptions, pub tls: bool,
        pub clock: Option<Arc<ManualClock>>, pub github_api_url: Option<String>, pub service_principals: Vec<(String, String /* plaintext secret */)>,
        pub tweak: Option<Arc<dyn Fn(&mut ServerConfig) + Send + Sync>>,
        pub registry: Option<Arc<dyn Fn() -> Registry + Send + Sync>> /* None = v1_default */ }
    impl Default for ServerTestOpts;   // acl: everyone in group eng reads/writes "/", admins = github:admin
    pub struct InProcessServer { pub addr: SocketAddr, pub endpoint: Endpoint, pub tls: Option<TlsOptions>, pub storage: Storage,
        pub mem: Option<MemHandles>, pub keyring: Keyring, pub policy_path: PathBuf, pub dir: tempfile::TempDir, .. }
    impl InProcessServer {
        pub async fn start(opts: ServerTestOpts) -> Self;
        pub fn ctx(&self) -> Arc<AppContext>;
        pub fn token_for(&self, p: &Principal) -> String;
        pub async fn client(&self, as_: Option<&Principal>) -> ScionClient;
        pub fn set_policy(&self, toml: &str) -> Result<()>;   // writes acl file + reload_now
        pub async fn kill(&mut self);                         // abort tasks, drop listener; storage survives
        pub async fn restart(&mut self);                      // new ServerApp over the same Storage, same address
    }
}
```

### 2.15 scion-cache

```rust
#[derive(Clone, Debug)] pub enum Payload { Raw(Bytes), Zstd(Bytes) }
pub struct DiskCache { .. }
impl DiskCache {
    pub fn open(dir: &Path, max_bytes: u64, low_watermark: f64, verify_on_read: bool) -> Result<Self>; // deletes temp files, rebuilds index
    pub fn get(&self, id: &ObjectId) -> Result<Option<Bytes /* raw, verified if verify_on_read */>>;   // mismatch => delete + Ok(None)
    pub fn get_payload(&self, id: &ObjectId) -> Result<Option<Payload>>;
    pub fn insert(&self, id: &ObjectId, p: Payload) -> Result<()>;   // temp in same dir, verify digest (INTEGRITY), fsync, rename; may evict
    pub fn remove(&self, id: &ObjectId) -> Result<()>; pub fn contains(&self, id: &ObjectId) -> bool; pub fn usage(&self) -> u64;
}
pub struct Coalescer<K, V> { .. }
impl<K: Eq + Hash + Clone + Send + Sync + 'static, V: Clone + Send + Sync + 'static> Coalescer<K, V> {
    pub fn new() -> Self; pub async fn get_or_fetch<F, Fut>(&self, k: K, f: F) -> Result<V> where F: FnOnce() -> Fut, Fut: Future<Output = Result<V>> + Send; }
pub struct UpstreamLimiter { .. }
impl UpstreamLimiter { pub fn new(max_concurrency: usize, max_queue: usize) -> Self; pub async fn acquire(&self) -> Result<LimiterPermit>; } // RESOURCE_EXHAUSTED on overflow
pub struct LimiterPermit;
pub struct RefCache { .. }
impl RefCache { pub fn new(ttl: Duration) -> Self; pub fn get(&self, name: &RefName, now: Instant) -> Option<(ObjectId, u64)>;
    pub fn observe(&self, name: &RefName, target: ObjectId, version: u64, now: Instant) -> (ObjectId, u64); } // never returns a lower version
pub struct LocalRedactions { .. }
impl LocalRedactions { pub fn new(now: Instant) -> Self; pub fn apply(&self, ids: &[ObjectId], epoch: u64, now: Instant);
    pub fn mark_refreshed(&self, epoch: u64, now: Instant); pub fn contains(&self, id: &ObjectId) -> bool; pub fn epoch(&self) -> u64;
    pub fn is_stale(&self, now: Instant, max: Duration) -> bool; }
pub struct AccessCache { .. }   // decisions never shared across principals; cleared on policy epoch change
impl AccessCache { pub fn new(ttl: Duration) -> Self; pub fn get(&self, principal: &str, id: &ObjectId, now: Instant) -> Option<bool>;
    pub fn put(&self, principal: &str, id: &ObjectId, allowed: bool, now: Instant);
    pub fn root_reader(&self, principal: &str, now: Instant) -> Option<bool>; pub fn set_root_reader(&self, principal: &str, v: bool, now: Instant);
    pub fn on_policy_epoch(&self, epoch: u64); }
```

### 2.16 scion-daemon

```rust
#[async_trait] pub trait UpstreamConnector: Send + Sync { async fn connect(&self) -> Result<ScionClient>; fn destinations(&self) -> Vec<String>; }
pub struct DirectUpstream { .. }  impl DirectUpstream { pub fn new(endpoint: Endpoint, tls: Option<TlsOptions>) -> Self; } // records destinations
#[derive(Clone, Debug, Default)] pub struct DaemonStats { pub upstream_requests: u64, pub cache_hits: u64, pub cache_misses: u64,
    pub fail_closed: bool, pub redaction_epoch: u64, pub policy_epoch: u64 }
pub struct Daemon;
impl Daemon { pub async fn start(cfg: DaemonConfig, upstream: Arc<dyn UpstreamConnector>, clock: Arc<dyn Clock>, metrics: Arc<Metrics>) -> Result<DaemonHandle>; }
pub struct DaemonHandle { .. }
impl DaemonHandle { pub fn socket_path(&self) -> &Path; pub fn endpoint(&self) -> Endpoint; pub fn stats(&self) -> DaemonStats;
    pub fn cache(&self) -> &DiskCache; pub fn block_redaction_polling(&self, on: bool) /* test hook */; pub async fn shutdown(self); }
#[cfg(feature = "testing")] pub mod testing {
    pub struct InProcessDaemon { pub handle: DaemonHandle, pub dir: tempfile::TempDir, pub cfg: DaemonConfig }
    impl InProcessDaemon { pub async fn start(upstream: Endpoint, tls: Option<TlsOptions>, tweak: impl FnOnce(&mut DaemonConfig)) -> Self;
        pub async fn client(&self, token: Option<String>) -> ScionClient; pub async fn kill(&mut self); pub async fn restart(&mut self); } }
```

### 2.17 scion-wc

```rust
pub mod points { /* P00: client fault point names */ pub const GOTO_AFTER_JOURNAL: &str = "wc.goto.after_journal"; pub const GOTO_MID_WRITE: &str = "wc.goto.mid_write";
  pub const COMMIT_AFTER_JOURNAL: &str = "wc.commit.after_journal"; pub const COMMIT_AFTER_UPLOAD: &str = "wc.commit.after_upload";
  pub const COMMIT_AFTER_CREATE: &str = "wc.commit.after_create"; pub const REBASE_AFTER_JOURNAL: &str = "wc.rebase.after_journal";
  pub const REBASE_MID_MEMBER: &str = "wc.rebase.mid_member"; pub const REBASE_AFTER_CONFLICT_WRITE: &str = "wc.rebase.after_conflict_write";
  pub const SPARSE_MID_WRITE: &str = "wc.sparse.mid_write"; pub const CHECKOUT_MID_WRITE: &str = "wc.checkout.mid_write"; }
pub mod fs_safe { pub struct SafeRoot { .. }
  impl SafeRoot { pub fn open(root: &Path) -> Result<Self>; pub fn write_file_atomic(&self, p: &RepoPath, data: &[u8], mode: Mode) -> Result<()>;
    pub fn symlink(&self, p: &RepoPath, target: &[u8]) -> Result<()>; pub fn remove(&self, p: &RepoPath) -> Result<()>;
    pub fn read(&self, p: &RepoPath) -> Result<Vec<u8>>; pub fn stat(&self, p: &RepoPath) -> Result<Option<index::StatTuple>>;
    pub fn read_link(&self, p: &RepoPath) -> Result<Vec<u8>>; pub fn is_case_insensitive(&self) -> bool; }
  pub fn validate_entry_path(p: &[u8]) -> Result<RepoPath>;               // 4.4 + never ".scion" in any case
  pub fn case_collisions(paths: &[RepoPath]) -> Vec<(RepoPath, RepoPath)>; } // ASCII case-fold comparison (deterministic on any fs)
pub mod index { #[derive(Clone, Debug, PartialEq, Eq)] pub struct StatTuple { pub size: u64, pub mtime_ns: i128, pub ctime_ns: i128, pub inode: u64, pub mode: u32 }
  #[derive(Clone, Debug, PartialEq, Eq)] pub struct IndexEntry { pub stat: StatTuple, pub id: ObjectId, pub mode: Mode, pub redacted: bool }
  pub struct StatCache { pub entries: BTreeMap<RepoPath, IndexEntry>, pub written_at_ns: i128 }
  impl StatCache { pub fn load(scion_dir: &Path) -> Result<Self>; pub fn save(&mut self, scion_dir: &Path) -> Result<()>; pub fn is_racy(&self, e: &IndexEntry) -> bool; } }
pub mod ignore { pub struct IgnoreMatcher { .. } impl IgnoreMatcher { pub fn load(root: &Path) -> Result<Self>; pub fn is_ignored(&self, p: &RepoPath, is_dir: bool) -> bool; } }
pub mod status {
  pub trait TreeLookup: Send + Sync { fn files_in_scope(&self, scope: &[RepoPath]) -> Result<BTreeMap<RepoPath, EntryInfo>>; }
  #[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)] pub struct StatusReport { pub modified: Vec<RepoPath>, pub added: Vec<RepoPath>,
     pub deleted: Vec<RepoPath>, pub mode_changed: Vec<RepoPath>, pub redacted_dirty: Vec<RepoPath>, pub hashed: usize }
  pub fn status(root: &fs_safe::SafeRoot, idx: &mut index::StatCache, ign: &ignore::IgnoreMatcher, scope: &[RepoPath],
                inaccessible: &[RepoPath], base: &dyn TreeLookup) -> Result<StatusReport>; }
pub mod state { #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
  pub enum PendingOperation { Checkout { target: ObjectId }, Goto { from: ObjectId, to: ObjectId }, SparseChange { old: Vec<RepoPath>, new: Vec<RepoPath> },
     Commit { parent: ObjectId, change_id: ChangeId, amend: bool }, Rebase { original: ObjectId, onto: ObjectId, remaining_stack: Vec<ObjectId>,
     done: Vec<ObjectId>, conflicts: Vec<Vec<u8>>, resolved: Vec<Vec<u8>>, current: Option<ObjectId> } }
  #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
  pub struct WcState { pub endpoint: Option<String>, pub base: ObjectId, pub tracking_ref: Option<RefName>, pub sparse: Vec<RepoPath>,
     pub inaccessible: Vec<RepoPath>, pub pending: Option<PendingOperation> }
  pub fn load(scion_dir: &Path) -> Result<WcState>; pub fn save(scion_dir: &Path, s: &WcState) -> Result<()>; } // canonical TOML, atomic
pub mod lock { pub struct WcLock; pub fn acquire(scion_dir: &Path) -> Result<WcLock>; }   // fs4 advisory, non-blocking
pub mod credentials { #[derive(Clone, Debug, PartialEq)] pub struct Credentials { pub token: String, pub github_token: Option<String> }
  pub fn write(path: &Path, c: &Credentials) -> Result<()>; pub fn read(path: &Path) -> Result<Credentials>; } // 0600; refuse group/world readable
pub const REDACTED_PLACEHOLDER_PREFIX: &str = "Scion: contents redacted (object ";
pub fn placeholder_bytes(id: &ObjectId) -> Vec<u8>;   // "Scion: contents redacted (object <id>).\n"
#[derive(Clone)] pub struct WcOptions { pub faults: Arc<FaultRegistry>, pub force_case_insensitive: bool, pub fetch_batch: usize }
pub struct WorkingCopy { .. }   // wc.rs (P18)
impl WorkingCopy {
    pub async fn checkout(client: ScionClient, root: &Path, rev: &str, sparse: Vec<RepoPath>, opts: WcOptions) -> Result<Self>;  // P18
    pub fn open(root: &Path, client: ScionClient, opts: WcOptions) -> Result<Self>;                                             // P18
    pub fn state(&self) -> &state::WcState; pub fn root(&self) -> &Path;
    pub async fn recover(&mut self) -> Result<Option<String>>;     // P18 (dispatches Commit/Rebase to P28 fns)
    pub async fn status(&mut self) -> Result<status::StatusReport>;                                      // P18
    pub async fn goto(&mut self, rev: &str, force: bool) -> Result<GotoReport>;                         // P18
    pub async fn sparse_add(&mut self, p: &[RepoPath]) -> Result<()>; pub async fn sparse_remove(&mut self, p: &[RepoPath], force: bool) -> Result<()>; // P18
    pub async fn refresh_redactions(&mut self) -> Result<Vec<RepoPath> /* dirty, reported */>;           // P18
    pub async fn diff(&mut self) -> Result<Vec<FileChangeView>>;                                          // P18
    pub async fn commit(&mut self, message: &[u8], author: &[u8], paths: Option<&[RepoPath]>) -> Result<ObjectId>; // P28
    pub async fn amend(&mut self, message: Option<&[u8]>) -> Result<ObjectId>;                            // P28
    pub async fn rebase(&mut self, onto: &str) -> Result<RebaseReport>; pub async fn rebase_abort(&mut self) -> Result<()>; // P28
    pub fn resolve(&mut self, p: &RepoPath, force: bool) -> Result<()>;                                   // P28
    pub async fn land(&mut self, r: &RefName) -> Result<LandReport>;                                      // P28
}
// report types (serde::Serialize + schemars::JsonSchema): GotoReport, RebaseReport, LandReport, FileChangeView
#[cfg(feature = "testing")] pub mod testing {   // P18 scenarios; P28 adds write scenarios in testing_write.rs (pub use from testing)
    pub struct Scenario { pub client: ScionClient, pub root: tempfile::TempDir }
    pub async fn sparse_object_set(s: &Scenario, rev: &str, dir: &RepoPath) -> Result<std::collections::BTreeSet<ObjectId>>; /* + one fn per P18 scenario */ }
```

### 2.18 scion-cli, 2.19 scion-import

```rust
// scion-cli
pub trait CliIo { fn stdout(&mut self) -> &mut dyn Write; fn stderr(&mut self) -> &mut dyn Write; fn stdin_line(&mut self) -> Option<String>; }
pub fn run(args: Vec<OsString>, env: &dyn scion_config::Env, io: &mut dyn CliIo) -> i32;   // testable entry; main() calls it
pub mod output { pub const SCHEMA_VERSION: u32 = 1;
  #[derive(serde::Serialize, schemars::JsonSchema)] pub struct Envelope<T> { pub schema: String /* "scion.cli.<cmd>.v1" */, pub data: T }
  pub fn write_schemas(dir: &Path) -> std::io::Result<Vec<PathBuf>>; pub fn command_names() -> &'static [&'static str]; }
// scion-import
#[derive(Clone, Debug)] pub struct ImportOptions { pub git_dir: PathBuf, pub git_ref: String, pub target_ref: RefName, pub skip_submodules: bool, pub batch_bytes: usize }
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)] pub struct ImportReport { pub commits_seen: u64, pub imported: u64, pub skipped_existing: u64,
    pub blobs_uploaded: u64, pub skipped_gitlinks: Vec<(String, String)>, pub final_target: Option<ObjectId>, pub ref_version: u64 }
pub async fn import(client: &ScionClient, opts: &ImportOptions) -> Result<ImportReport>;
pub fn change_id_for(git_commit: &[u8; 20]) -> ChangeId;        // BLAKE3("scion-import-v1" || id)[0..16]
pub fn map_mode(git_mode: u32) -> Result<Option<Mode>>;         // 100644,100664->File 100755->Executable 120000->Symlink 040000->None(tree) 160000->Err/skip
```

### 2.20 scion-refmodel (no scion deps; `blake3` only)

```rust
pub type MPath = Vec<u8>;
#[derive(Clone, Debug, PartialEq, Eq, Hash)] pub struct MFile { pub mode: u8 /* 1..=3 */, pub content: Vec<u8> }
pub type MTree = BTreeMap<MPath, MFile>;                         // file-level, structural (no ids needed)
#[derive(Clone, Debug, PartialEq, Eq)] pub enum MDelta { Put(MPath, MFile), Delete(MPath) }
#[derive(Clone, Debug, PartialEq, Eq)] pub struct MConflict { pub path: MPath, pub kind: &'static str }
pub mod enc { pub fn blob_id(c: &[u8]) -> [u8; 34]; pub fn tree_id(t: &MTree) -> [u8; 34]; pub fn tree_bytes_for_dir(t: &MTree, dir: &[u8]) -> Vec<u8>;
  pub fn meta_bytes(m: &MMeta) -> Vec<u8>; pub fn changeset_bytes(c: &MChangeset) -> Vec<u8>; pub fn id_text(id: &[u8; 34]) -> String; }
pub struct MMeta { pub author: Vec<u8>, pub author_time: (i64, i64), pub committer: Option<Vec<u8>>, pub committer_time: Option<(i64, i64)>,
    pub creator: Vec<u8>, pub message: Vec<u8>, pub ext: Vec<(u64, Vec<u8>)> }
pub struct MChangeset { pub parents: Vec<[u8; 34]>, pub root_tree: [u8; 34], pub meta: [u8; 34], pub change_id: [u8; 16], pub delta: Vec<(MPath, Option<(u8, [u8; 34], u64)>)> }
pub fn apply_delta(t: &MTree, d: &[MDelta]) -> Result<MTree, &'static str /* subcode */>;
pub fn file_diff(a: &MTree, b: &MTree) -> Vec<MDelta>;
pub mod diff { pub fn myers(a: &[&[u8]], b: &[&[u8]]) -> Vec<(char /* '=' '-' '+' */, usize, usize)>; pub fn split_lines(b: &[u8]) -> Vec<&[u8]>;
  pub fn merge3(b: &[u8], o: &[u8], t: &[u8]) -> Result<Vec<u8>, Vec<(usize, usize)>>; }
pub fn merge_trees(b: &MTree, o: &MTree, t: &MTree, paths: Option<&BTreeSet<MPath>>, max_text: u64, redacted: &BTreeSet<Vec<u8>>) -> Result<MTree, Vec<MConflict>>;
pub struct MRepo { .. }   // commits indexed 0..n
impl MRepo { pub fn new() -> Self; pub fn commit(&mut self, parents: &[usize], tree: MTree) -> usize; pub fn tree(&self, c: usize) -> &MTree;
    pub fn generation(&self, c: usize) -> u64; pub fn skip(&self, c: usize) -> Vec<usize>; pub fn is_ancestor(&self, a: usize, b: usize) -> bool;
    pub fn merge_bases(&self, a: usize, b: usize) -> Vec<usize>; pub fn rebase(&mut self, c: usize, onto: usize, max_text: u64) -> Result<usize, MRebaseError>;
    pub fn log_path(&self, from: usize, p: &[u8], first_parent_only: bool) -> Vec<usize>;   // TREESAME via node-reuse semantics
    pub fn blame(&self, c: usize, p: &[u8]) -> Vec<(u32, u32, usize, u32)>; pub fn all_versions(&self, c: usize, p: &[u8]) -> Vec<Vec<u8>>; }
pub enum MRebaseError { Conflict(Vec<MConflict>), Empty }
pub fn land_sequential(base: &MTree, changes: &[Vec<MDelta>]) -> Result<MTree, (usize, Vec<MConflict>)>;
pub fn materialize(t: &MTree, sparse: &[MPath]) -> MTree;
```

### 2.21 scion-testkit

```rust
// corpus.rs, shared.rs, edge.rs, encode.rs, git.rs, lincheck.rs (P09)
#[derive(Clone, Debug)] pub struct CorpusSpec { pub files: u64, pub depth: u32, pub fanout: u32, pub history_len: u32, pub merge_ratio: f64,
    pub octopus_ratio: f64, pub binary_ratio: f64, pub symlink_ratio: f64, pub seed: u64 }
#[derive(Clone, Debug)] pub struct GenCommit { pub parents: Vec<usize>, pub tree: scion_refmodel::MTree, pub author: Vec<u8>, pub message: Vec<u8>, pub change_id: [u8; 16] }
#[derive(Clone, Debug)] pub struct GenRepo { pub commits: Vec<GenCommit> }   // topological order
pub fn generate(spec: &CorpusSpec) -> GenRepo;
pub struct SharedSubtreeRepo { pub depth: u32, pub fanout: u32, pub files: u64, .. }   // 10^3..10^7 logical files via identical-subtree dedup
pub fn shared_subtree_repo(files: u64, depth: u32) -> SharedSubtreeRepo;
impl SharedSubtreeRepo { pub fn encoded_objects(&self) -> Vec<(ObjectId, Kind, Bytes)>; pub fn root_tree(&self) -> ObjectId;
    pub fn perturb_path(&self) -> RepoPath; pub fn distinct_trees(&self) -> usize; }
pub fn edge_cases() -> Vec<(&'static str, GenRepo)>;
pub fn encode_tree(t: &scion_refmodel::MTree) -> (ObjectId, Vec<(ObjectId, Kind, Bytes)>);  // production encoding (scion-object) of a structural tree
pub mod git { pub fn bundles() -> Vec<PathBuf>; pub fn unpack(bundle: &Path, dst: &Path) -> Result<PathBuf>; pub fn git_available() -> bool;
    pub struct GitFixture { pub dir: tempfile::TempDir } impl GitFixture { pub fn build(script: &[GitOp]) -> Result<Self>; }
    pub enum GitOp { /* commit with files/modes/gitlinks, merge (incl. octopus), orphan root, empty commit */ } }
pub mod lincheck { #[derive(Clone, Debug)] pub enum RefOp { Read, Cas { expect: u64, target: [u8; 34] }, Land { change: [u8; 16] } }
    #[derive(Clone, Debug)] pub enum RefRet { Value { version: u64, target: [u8; 34] }, Mismatch, Ok { version: u64, target: [u8; 34] }, Fail, Unknown }
    pub struct HistoryRecorder { .. } impl HistoryRecorder { pub fn new() -> Self; pub fn invoke(&self, client: u32, r: &str, op: RefOp) -> OpToken;
        pub fn complete(&self, t: OpToken, ret: RefRet); pub fn events(&self) -> Vec<Event>; }
    pub fn check_ref_registers(events: &[Event]) -> Result<(), Violation>; }   // WGL search, partitioned per ref, Unknown = may or may not have happened
// fake_upstream.rs + tls.rs (P08), fake_github.rs (P06)
pub struct FakeUpstream { .. }   // in-memory scion.v1 ObjectService (+ GetChangesets/Resolve, CheckAccess/WhoAmI, ListRedactions); knobs below
impl FakeUpstream { pub async fn start(opts: FakeUpstreamOpts) -> Self; pub fn endpoint(&self) -> Endpoint; pub fn tls(&self) -> Option<TlsOptions>;
    pub fn insert_object(&self, id: ObjectId, raw: Bytes, restricted: bool);  pub fn insert_changeset(&self, id: ObjectId, encoded: Bytes);
    pub fn set_ref(&self, name: &str, target: ObjectId, version: u64); pub fn deny(&self, principal: &str, id: ObjectId);
    pub fn redact(&self, id: ObjectId); pub fn corrupt(&self, id: ObjectId); pub fn kill_stream_after_bytes(&self, n: Option<u64>);
    pub fn set_down(&self, down: bool); pub fn set_epochs(&self, policy: u64, redaction: u64);
    pub fn requests(&self) -> Vec<RecordedRequest>; pub fn reset_requests(&self); }
pub struct FakeUpstreamOpts { pub tls: bool, pub principal_of: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>, pub public_keys: Vec<(String, [u8; 32])> }
pub struct FakeGitHub { .. }
impl FakeGitHub { pub async fn start() -> Self; pub fn api_url(&self) -> String; pub fn oauth_base(&self) -> String;
    pub fn add_user(&self, token: &str, login: &str, id: u64); pub fn approve_device_code(&self, user_code: &str, token: &str); pub fn set_down(&self, down: bool); }
pub struct TestTls { pub ca_pem: Vec<u8>, pub cert_pem: Vec<u8>, pub key_pem: Vec<u8> }
impl TestTls { pub fn generate() -> Self; pub fn client_options(&self) -> TlsOptions; }   // SAN localhost + 127.0.0.1
```

### 2.22 scion-conformance (P30)

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Via { Server, Daemon }
#[derive(Clone, Debug)] pub enum Backend { Memory, External }   // External only when SCION_TEST_CRDB_URL and SCION_TEST_S3_ENDPOINT are set
impl Backend { pub fn from_env() -> Vec<Backend>; }             // always [Memory], plus External when available
pub struct ClusterOpts { pub backend: Backend, pub servers: usize, pub daemon: bool, pub acl_toml: String, pub server: ServerTestOpts, pub daemon_tweak: Option<Arc<dyn Fn(&mut DaemonConfig) + Send + Sync>> }
pub struct TestCluster { pub servers: Vec<InProcessServer>, pub daemon: Option<InProcessDaemon>, pub github: FakeGitHub, pub storage: Storage, pub logs: LogCapture }
impl TestCluster { pub async fn start(o: ClusterOpts) -> Self; pub async fn client(&self, via: Via, who: &Principal) -> ScionClient;
    pub fn token_for(&self, who: &Principal) -> String; pub fn set_policy(&self, toml: &str) -> Result<()>;
    pub async fn seed(&self, repo: &GenRepo) -> Vec<ObjectId>; pub fn counters(&self) -> CounterSnapshot;
    pub async fn kill_server(&mut self, i: usize); pub async fn restart_server(&mut self, i: usize); }
pub mod invariants { pub async fn check_16_2(c: &TestCluster) -> Result<()>; } // fsck + ref-log provenance + digest checks on everything served
```

## 3. Testing strategy

### 3.1 Hermetic by default

`cargo test --workspace` needs no external service, no network and no Docker. It uses:

- `Storage::in_memory` (MemMetaStore + MemBlobStore).
- `InProcessServer` on loopback with TLS. Certificates come from `TestTls` (rcgen), so SPEC 17 is exercised even in tests.
- `InProcessDaemon` on a temp Unix socket.
- `FakeGitHub` (axum on loopback) and `FakeUpstream`.
- Temp directories for working copies and caches.

The `git` CLI is optional. It is used only by differential tests (`git check-ignore`, `git ls-tree`, `git blame`, `git merge-file`) and by fixture builders. Tests that need it skip with a printed `SKIP:` line when `git` is absent. The vendored bundles are read with `gix`, so 19.14 fidelity still runs without the git CLI, using a gix-based tree walk as the comparison side.

### 3.2 The in-memory backends are a faithful model

- **Strictly serializable.** Each `MetaStore` method is one transaction. MemMetaStore runs it under one mutex over `im` persistent maps, which makes it strictly serializable. CAS mismatch, `ON CONFLICT DO NOTHING`, and the `landed(ref, change_id)` unique violation (which rolls back the whole `cas_ref`) are reproduced exactly, per `docs/contracts/storage-semantics.md`.
- **Stale reads.** A ring of the last K committed snapshots serves `Consistency::Stale`, controlled by `stale_lag_commits` and `advance_stale_view()`. This lets the SPEC 5.4 rule (a stale miss is retried with a strong read before NOT_FOUND) be tested for real.
- **Forced transaction failures.** `fail_txn_every` makes every Nth transaction fail with `UNAVAILABLE/txn_retry` and no effect. This models CockroachDB 40001 exhaustion, so the hermetic tests check that no caller leaves partial state.
- **Fault decorators.** `InstrumentedMeta` / `InstrumentedBlob` wrap every backend, CRDB and S3 included, with points `meta.<method>.before|after` and `blob.<method>.before|after`. `after` gives fail-after-commit. History, Redactor and the client also call named checkpoints (`scion_core::fault::points`) between every SPEC 5.3 step. Together these cover 19.0 Fault Hooks.
- **Shared suites.** `scion_store::conformance::{meta_store_suite, blob_store_suite}` is a single suite run against Mem (always) and against CRDB/S3 (gated), so both backends implement identical semantics.

### 3.3 External backends (gated)

| Env var | Meaning |
|---|---|
| `SCION_TEST_CRDB_URL` | e.g. `postgresql://root@localhost:26257/defaultdb?sslmode=disable`. Each test calls `CrdbMetaStore::connect_fresh_for_tests`, which creates database `scion_test_<random>`. |
| `SCION_TEST_S3_ENDPOINT` (gate), `SCION_TEST_S3_BUCKET` (default `scion-test`), `SCION_TEST_S3_ACCESS_KEY`, `SCION_TEST_S3_SECRET_KEY`, `SCION_TEST_S3_REGION` (default `us-east-1`) | MinIO or S3. Each run uses a unique key prefix. |

When the variables are unset, gated tests print `SKIP: SCION_TEST_CRDB_URL unset` and return early, so they still pass. Commands:

- `docker compose -f docker/compose.test.yml up -d` starts CockroachDB v24.x single-node (insecure) and MinIO.
- `scripts/test-external.sh` exports the variables and runs `cargo test --workspace -- external_`.

Gated tests are named `external_*`. `TestCluster` (`Backend::from_env`) re-runs the conformance suites on the External backend when both variables are set. CI runs the external job nightly.

### 3.4 Oracles

1. **Reference Model** (`scion-refmodel`):
   - Written from SPEC text alone, by agents who are forbidden from reading production crates.
   - It has its own encoder and its own Myers implementation built from `docs/contracts/line-diff3-v1.md`.
   - `check-layers.sh` verifies it has no scion-* dependency of any kind.
   - It works on structural values (path -> (mode, bytes) maps), so comparisons are id-independent. Where ids matter, both sides compute ids from canonical bytes.
2. **Golden vectors.**
   - `testdata/golden/objects/*.json` (P01): hex bytes plus id text for every kind.
   - `testdata/golden/diff/*.json` (P02): Myers and merge3 vectors, at least 60 cases.
   - Both are frozen: a test fails if any file changes. P17 checks that the refmodel reproduces them.
3. **fsck** (`scion_store::fsck`) checks the SPEC 4.5 invariants and is the oracle for every crash-injection test (19.3, 19.16, 16.2).
4. **Linearizability checker** (`scion_testkit::lincheck`): WGL search over per-ref versioned CAS registers. Histories are capped at about 200 operations, and many seeds are run.
5. **Whole-system differential** (P32): seeded random operation sequences (create, rebase, merge, land, update_ref, redact) are run against the real server and against the refmodel. Trees, conflict sets, ref-log order and error codes are compared.
6. **External differentials:** `git check-ignore`, `git ls-tree`, `git blame`, and the RECOMMENDED `git merge-file` check (with documented disagreements).

### 3.5 Scaling of heavy counts

Default runs use `proptest_cases(N)`: about 10^4 cases where SPEC demands 10^6, and 10^3 where it demands 10^5. `SCION_PROPTEST_CASES` overrides the count.

Spec-sized runs live in `#[ignore]` tests named `full_*`:

- 10^6-case round trips and merge properties.
- 10^5 lazy derivation chain and graph queries.
- N=1000 concurrent lands.
- 10^7-file complexity gates.

`scripts/check.sh --full` runs them with `SCION_FULL=1 cargo test --release -- --include-ignored`. Conformance is claimed only on a `--full` run, and CI runs `--full` nightly.

The 10^7-file gates stay feasible in memory because `shared_subtree_repo` exploits content addressing. A balanced tree of identical subtrees, perturbed along one path, needs O(depth x fanout) distinct objects. The gates assert counters only, never wall-clock time.

Profiles: `[profile.test] opt-level = 1`, `[profile.test.package."*"] opt-level = 3`, `[profile.dev.package."*"] opt-level = 2`.

### 3.6 Where tests live

- **Unit tests** go inside the owning module file (`#[cfg(test)]`).
- **Crate integration tests** go in `crates/<crate>/tests/<prefix>_*.rs`. The prefix is listed in the package's `owns`. Shared helpers go in `tests/<prefix>_support/mod.rs`, owned by the same package.
- **Cross-cutting 19.x suites** go in `crates/scion-conformance/tests/*.rs`, one file per owning package.
- **Reusable scenario functions** (for re-running client and import suites against the real stack) live in `src/testing.rs` behind a `testing` feature. They do not live in `tests/`, because integration-test modules cannot be imported across crates.

Appendix A maps every SPEC 19.x item to the package that writes it.

## 4. Work packages and waves

This section is rendered from `docs/PACKAGES.json` (the source of truth for the orchestrator). Sizes: S < 0.5 day, M about 1 day, L about 2-3 days, XL > 3 days of agent effort.

Ownership notation: `{a,b}` expands to several files. A `crates/<c>/src/**` or `crates/<c>/**` entry NEVER includes `Cargo.toml` or `src/lib.rs`, which stay P00-owned and frozen. A package may create new private submodule files under the directory of a module file it owns (e.g. `src/create/*.rs`). Every package's deliverables end with the common definition of done (§0).

### 4.1 Wave summary

| Wave | Packages (parallel) | Count |
|---|---|---|
| 0 | P00 foundation and frozen stubs | 1 |
| 1 | P01 object codec, P02 merge engine, P03 store + mem backends, P04 config + obs, P05 policy, P06 auth + FakeGitHub, P07 refmodel, P08 proto + FakeUpstream, P09 testkit corpus + lincheck, P10 cache, P11 wc local | 11 |
| 2 | P12 history create, P13 history graph, P14 derived framework, P15 crdb + s3 (gated), P16 daemon, P17 oracle cross-check, P18 wc fetch/checkout/goto/sparse | 7 |
| 3 | P19 rebase + merge, P20 refs + land, P21 history nodes + git_mapping, P22 redaction, P23 server core | 5 |
| 4 | P24 blame/log + Log/Blame RPCs, P25 object/admin RPCs, P26 write/ref RPCs, P27 query RPCs | 4 |
| 5 | P28 wc commit/rebase/land, P29 import, P30 conformance A + TestCluster | 3 |
| 6 | P31 CLI, P32 conformance B1 (concurrency), P33 conformance B2 (failures, gates) | 3 |

Total: 34 packages in 7 waves (0-6).

Critical path (7 waves): P00 -> P03 (store) -> P12 (CreateChangeset) -> P19/P20 (rebase, land; parallel thanks to the Rebaser seam) -> P26 (write/ref RPCs) -> P28 (working-copy commit/rebase) -> P31 (CLI). Off the critical path: the daemon (wave 2, against FakeUpstream), client fetch/checkout (wave 2, against FakeUpstream), the oracle cross-check (wave 2), and the importer and conformance suites.

Dependency rule: every `depends_on` entry is in a strictly earlier wave (checked mechanically when PACKAGES.json was generated). A package's tests may only exercise code from packages in its transitive `depends_on`. Code owned by same-wave packages is still `todo!()` at runtime.

### 4.2 Package details

#### Wave 0

##### P00: Foundation: workspace, frozen API stubs for all crates, scion-core, scion.v1 proto, contract docs, CI scripts

- **Size:** XL  |  **Depends on:** none
- **SPEC sections:** 3.2, 3.4, 4.1-4.3 (type shapes), 4.2 (ids and text form), 5.2 (row types), 6.5 (Limits), 7.1-7.8 (complete scion.v1 proto), 7.2 (error table), 8.3 (line-diff3-v1 contract), 9.2 (commit_graph formula), 18.5, 19.0 (fault registry, checkpoint names), 20.1
- **Owns:** `Cargo.toml`; `Cargo.lock`; `rust-toolchain.toml`; `rustfmt.toml`; `clippy.toml`; `.cargo/config.toml`; `.gitignore`; `crates/*/Cargo.toml (all 23 crates)`; `crates/*/src/lib.rs (all 23 crates)`; `crates/scion-proto/build.rs`; `proto/scion/v1/scion.proto`; `crates/scion-core/**`; `crates/scion-history/src/{hooks.rs,config.rs,types.rs,graph_compute.rs}`; `crates/scion-derived/src/types.rs`; `crates/scion-server/src/{grpc.rs,handlers/mod.rs,authz/mod.rs}`; `crates/scion-wc/src/points.rs`; `initial stub content of every other module file listed in PACKAGES.json (ownership of each transfers to the listed package)`; `scripts/{check.sh,check-layers.sh,layers.toml,test-external.sh}`; `docker/compose.test.yml`; `.github/workflows/ci.yml`; `docs/contracts/{line-diff3-v1.md,storage-semantics.md,amendments.md}`; `docs/implementation-defined.md`; `docs/testing.md`; `testdata/README.md`
- **Deliverables:** Create the whole Cargo workspace so that it compiles with every public API of docs/PLAN.md section 2 present. Concretely: (1) Root Cargo.toml: resolver = 3, edition 2024, an explicit members list of the 23 crates in PLAN 1.2. It has [workspace.package] and [workspace.lints] (rust unsafe_code = deny; clippy unwrap_used/expect_used/panic = deny, with clippy.toml allow-unwrap-in-tests/allow-expect-in-tests = true). It has [workspace.dependencies] pinning EXACT versions, resolved at scaffold time, of every third-party crate in PLAN 5.3, plus the profiles in PLAN 3.5. Generate and commit Cargo.lock. rust-toolchain.toml pins the installed 1.98 toolchain with rustfmt and clippy. (2) Every crate Cargo.toml declares all internal deps from PLAN 1.2 and all third-party deps it will need (PLAN 5.3 column 'used by'), dev-deps included, so later packages never edit manifests. It also declares features: scion-server/testing, scion-daemon/testing, scion-wc/testing, scion-import/testing, scion-auth/test-hooks. Bins: scion-server, scion-daemon, scion (crate scion-cli), scion-import; each src/main.rs is a stub. (3) Fully implement scion-core exactly as PLAN 2.1, with unit tests: error table, ids and text form, empty_tree_id, Mode, conflict types, Principal, Tier, Clock/ManualClock, Limits defaults, FaultRegistry semantics, points, trace, test_support. (4) Write proto/scion/v1/scion.proto completely per PLAN 2.13 (all six services, every message). crates/scion-proto/build.rs uses tonic-prost-build (protoc is on PATH) and emits a file descriptor set. The v1 module and FILE_DESCRIPTOR_SET are real; the other scion-proto modules are stubs. (5) Every other crate gets lib.rs and the module files named in PACKAGES.json owns lists. Each holds every PLAN section 2 item with rustdoc that cites the SPEC section. Bodies follow PLAN 2.0: todo!("<owning package id>") for behaviour; real assembly constructors and accessors (History::new, ObjectRepo::new, Storage::new, Storage::in_memory, MemMetaStore::new, MemBlobStore::new, DerivedEngine::new, StoreDeriveCtx::new, Registry::empty, Redactor::new, ObjectAuthorizer::new, RedactionSet::new); real trivial impls of NoEager and AllowAll; real compute_commit_graph (9.2: generation = 1 + max parent generation, root 1; skip[k] = first-parent ancestor at distance 2^k); no-op handlers::admin::spawn_background. Handler stubs return Err(Status::unimplemented("Pxx")). scion-server/src/grpc.rs is real: it implements the six generated tonic server traits, and each method delegates in one line to the handlers::<file>::<rpc> function listed in PLAN 2.14. (6) docs/contracts/line-diff3-v1.md (normative, PLAN 6.3): line splitting. Myers forward greedy O(ND) with the exact k-selection rule: move down (insert) when k == -d or (k != d and V[k-1] < V[k+1]), else move right (delete). Snake extension, storage of V per d, backtracking with the same rule, no heuristics, no cost cap. Edit-script normalization (within a changed run, deletions are listed before insertions). Hunk extraction. diff3 region building (transitive closure of b->o and b->t hunks whose base ranges overlap or touch, where an empty insertion range touching or inside another range counts as touching). A one-sided region applies. A two-sided region applies once if both sides produce identical text, otherwise it conflicts. Result assembly. At least 12 hand-worked examples, including tie-sensitive ones (swap AB/BA, repeated lines, empty inputs, unterminated last line, CRLF). (7) docs/contracts/storage-semantics.md: for every MetaStore method, the transaction boundary, reads, writes, uniqueness/conflict behaviour, outcome mapping, the CockroachDB DDL/SQL shape (all SPEC 5.2 tables, plus audit_log and secondary indexes: changeset id prefix, derived source_object, redactions epoch, redactions purge_after) and notes on the Mem implementation. Also ObjectRepo write ordering and the key layout. (8) docs/contracts/amendments.md (protocol plus an empty log). docs/implementation-defined.md: transcribe PLAN section 6 verbatim as the documented implementation-defined behaviours. docs/testing.md: summary of PLAN section 3 with env vars and commands. testdata/README.md. (9) Scripts. scripts/check.sh runs fmt check, build, test, clippy -D warnings and check-layers; --full sets SCION_FULL=1 and runs cargo test --release -- --include-ignored. scripts/check-layers.sh plus scripts/layers.toml implement PLAN 1.3 from cargo metadata, with a --self-test mode proving an injected upward edge is rejected. scripts/test-external.sh. docker/compose.test.yml: cockroachdb single-node insecure on 26257 and MinIO on 9000, with bucket creation. .github/workflows/ci.yml: check.sh on push; nightly --full and external jobs. (10) Before finishing, self-review against SPEC.md and PLAN section 2. cargo doc --workspace --no-deps must be warning-free. Gate G0 (a fresh-agent contract review) follows this package. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** scion-core unit tests: ErrorCode::ALL maps to the exact SPEC 7.2 gRPC status and retryable flag (19.7). ObjectId text form round-trips for every kind; ids with equal digests and different kinds are unequal and print differently (19.1). parse_text rejects bad input. ChangeId hex round-trips. empty_tree_id equals BLAKE3 of [0x01,0x00,0x00] with kind Tree. Principal parse/display. FaultRegistry: arm with times, hits counted when unarmed, Crash latch until reset, Delay. compute_commit_graph on hand-built rows (root generation 1; skip pointers at distances 1, 2, 4, 8 on a 10-chain; merge generation = 1 + max). Limits::default matches SPEC 6.5. scripts/check-layers.sh passes, and --self-test proves an upward edge is rejected. cargo build, clippy and doc are clean.

#### Wave 1

##### P01: scion-object: canonical codec, entities, path/ref/revision grammar, golden vectors

- **Size:** M  |  **Depends on:** P00
- **SPEC sections:** 4.1, 4.2 (ids inside objects), 4.3.1-4.3.4, 4.3.6 (ref name grammar), 4.4, 9.2 (HistoryNode encoding, derived kind 0x10), 11.8 (revision grammar parsing), 17 (decoded-form validation)
- **Owns:** `crates/scion-object/src/{enc.rs,canonical.rs,extensions.rs,path.rs,tree.rs,meta.rs,changeset.rs,history_node.rs,refname.rs,revision.rs,flat.rs}`; `crates/scion-object/tests/**`; `testdata/golden/objects/**`
- **Deliverables:** Implement PLAN 2.2 completely. Encoder/Decoder: uvarint is minimal LEB128 of at most 10 bytes, with overflow rejected; svarint is zigzag; bytes/list lengths may not exceed the remaining input; option tag must be 0 or 1; finish() rejects trailing bytes. All errors are INVALID_ARGUMENT with the scion-core decoding subcodes. Extensions: strictly ascending unique tags. Unknown odd tags are rejected (unknown_critical_extension). Even tags are preserved byte-for-byte; v1 has no known odd tags. Tree: format_version must be 1 (else unknown_format_version). Entries are strictly ascending by bytewise name and unique; names obey 4.4. Modes 1-3 require a Blob id, mode 4 requires a Tree id with size 0. validate_non_root rejects an empty tree (empty_tree). Meta: fields in SPEC order; Timestamp is a pair of svarints. Changeset: parents of kind Changeset, unique; root_tree of kind Tree; meta of kind Meta; change_id is 16 raw bytes; delta strictly ascending by path, op 1 = Put (mode 1..=3, id of kind Blob, size) and 2 = Delete. validate_shape(limits) checks parent count/uniqueness, delta sort/uniqueness, max_delta_entries and path rules (TOO_LARGE for limits, with a Limit payload). HistoryNode: format_version 1, kind u8 (1 File, 2 Dir), content, changeset, parents list, children list<(name, id)> ascending (must be empty for File), extensions. KIND = Derived(0x10). Name/RepoPath: 1-255 bytes, no NUL or '/', not '.' or '..', not '.scion' under ASCII case-insensitive comparison; paths are at most 4096 bytes with no leading/trailing '/' and no empty component; the root is the empty path; ancestors() and is_ancestor_of are component-wise. RefName: ^[a-z0-9][a-z0-9._/-]{0,199}$, with no '..', no '//' and no trailing '/'. parse_revision covers: full id text; 'c1' plus 8..63 hex (IdPrefix); a bare name, treated as a ref name or a bare hex prefix of at least 8 digits, decided later by the server; '<ref>@<u64>'; 'git:<40 hex>'; and chains of '~n' / '^k' suffixes (a bare '~' or '^' means 1; ^0 is invalid). flat::file_diff implements SPEC 4.3.4 FileDiff over FlatTree. Golden vectors: testdata/golden/objects/*.json with at least 14 cases: empty tree; a tree with all four modes; a tree with an even extension; a Meta with and without committer; a Meta with extension tag 2; the root changeset; a changeset with 2 parents and mixed Put/Delete; a changeset with an even extension; HistoryNode File and Dir; a blob. Each file is {name, kind, description, hex, id_text}. A test re-encodes every case from its constructor in Rust and compares bytes and id. A second test asserts a BLAKE3 digest constant over the sorted golden file contents, so any edit to the files fails CI (19.1 freeze). Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.1: golden vectors frozen (encode equality plus digest constant). Round trip decode(encode(x)) == x and encode(decode(b)) == b with proptest over random valid Tree/Meta/Changeset/HistoryNode. The default count is proptest_cases(10_000); a #[ignore] full_ test runs 10^6. One rejection test per class: non-minimal varint, 11-byte varint, length overrun, trailing bytes, unsorted entries, duplicate entries, unknown format_version, unknown odd extension tag, invalid mode, mode/kind mismatch. An unknown even tag is preserved byte-for-byte and changes the id. Equal digests with different kinds are never confused (a Tree id is rejected where a Blob id is required). 19.2 path-rule violations at parse level: '.', '..', '/', NUL, '.scion'/'.SCION'/'.Scion', 256-byte name, 4097-byte path, leading/trailing '/'. Ref-name grammar table. Revision grammar parse table (at least 25 cases, valid and invalid). file_diff unit cases (file<->dir transitions produce Delete plus Put).

##### P02: scion-merge: normative Myers diff and line-diff3-v1 engine

- **Size:** M  |  **Depends on:** P00
- **SPEC sections:** 8.3 (content merge, binary detection, decision table), 9.2 (blame line mapping), 7.4 (Diff hunks), 11.7 (marker format), 19.4
- **Owns:** `crates/scion-merge/src/{split.rs,myers.rs,hunks.rs,merge3.rs,binary.rs,classify.rs,markers.rs,origins.rs,unified.rs}`; `crates/scion-merge/tests/**`; `testdata/golden/diff/**`
- **Deliverables:** Implement PLAN 2.5 exactly according to docs/contracts/line-diff3-v1.md. That document is normative; if it is ambiguous, follow its worked examples and file an amendment. Do not use any third-party diff crate in src (similar is allowed only as a dev-dependency to cross-check edit-script minimality). split_lines keeps terminators. myers is the forward greedy O(ND) algorithm with the contract's k-selection rule and backtracking. diff_lines groups non-equal runs into hunks. is_binary: a NUL within the first 8000 bytes. merge3: region building, one-sided apply, identical two-sided apply-once, conflicts reported as ConflictHunk line ranges (base/ours/theirs, u32), and byte-exact output (no CRLF normalization, unterminated final lines preserved). classify implements 8.3 rows 1-6 in order: o==b take t; t==b take o; o==t take o; b absent with o and t present and different gives AddAdd; exactly one side absent and the other differing from b gives ModifyDelete; any side a symlink with sides differing gives Symlink; otherwise NeedContent. merge_mode applies the first three rows to modes. render_markers writes the exact 11.7 marker lines. line_origins maps child lines to parent lines via diff_lines (for blame). unified_hunks produces unified-diff hunks with context. Produce at least 60 golden vectors in testdata/golden/diff/*.json: diff cases {a_hex, b_hex, edits} and merge cases {base_hex, ours_hex, theirs_hex, result_hex | conflicts}. Include all worked examples from the contract doc and tie-break-sensitive inputs. Freeze them with a digest-constant test. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.4 content level: o==b => t; t==b => o; o==t => o; non-overlapping, non-adjacent hunks from both sides both apply; overlapping hunks conflict; adjacent hunks (zero unchanged lines between) conflict; identical hunks apply once. Each property runs with proptest_cases(10_000), with #[ignore] full_ variants at 10^6. Binary boundary: NUL at byte 7999 is binary, at byte 8000 is text. CRLF and unterminated-final-line merges are byte-exact. classify has one test per 8.3 table row. Myers optimality: edit length equals an O(N*M) LCS-based minimum on small random inputs. Determinism: the same input always gives the same output. Golden vectors are frozen. RECOMMENDED: a differential against `git merge-file -p` on clean text merges (skipped when git is absent), with disagreements recorded in crates/scion-merge/tests/GIT_MERGE_FILE_DIFFERENCES.md.

##### P03: scion-store: in-memory MetaStore/BlobStore, ObjectRepo, counters and fault decorators, fsck, backend conformance suites

- **Size:** L  |  **Depends on:** P00
- **SPEC sections:** 5.1, 5.2, 5.3 step 1, 5.4, 4.5 (fsck), 9.1 (leases), 13.2 (redaction transactions and byte moves), 16.1 classes 2/3/7, 17 (audit log, decoded-form validation), 19.0 (counters, fault hooks), 19.3
- **Owns:** `crates/scion-store/src/{types.rs,meta.rs,blob.rs,storage.rs,keys.rs,mem_meta.rs,mem_blob.rs,repo.rs,instrument.rs,fsck.rs,conformance.rs}`; `crates/scion-store/tests/**`
- **Deliverables:** Implement PLAN 2.3 and docs/contracts/storage-semantics.md. MemMetaStore: all state is im persistent maps behind one parking_lot::Mutex, and each trait method is one atomic step (strictly serializable). Required behaviour: - ON CONFLICT DO NOTHING for objects, changesets, children, object_paths and derived; commit_changeset inserts every row independently, so a re-commit fills in missing rows. - cas_ref: RefExpect::Absent or Version(v); on success version becomes v+1 (1 on create); ref_log row, landed rows and audit row in the same step; a landed (ref, change_id) unique violation returns LandedConflict and applies nothing; a version mismatch returns the current row. - redact: increments counters.redaction_epoch, inserts rows stamped with the new epoch, sets objects.state = quarantined. unredact: Quarantined only (else INVALID_ARGUMENT/not_quarantined), epoch+1, state live. mark_purged: clears inline_data, state purged. - Leases are TTL-based against the provided now. - A ring of the last K committed snapshots serves Consistency::Stale (stale_lag_commits, set_stale_lag, advance_stale_view). - fail_txn_every: every Nth transaction fails with UNAVAILABLE/txn_retry and no effect. MemBlobStore supports corrupt(), set_down() and keys(). keys.rs implements the SPEC 5.1 layout. InstrumentedMeta/InstrumentedBlob count meta_txns, meta_rows_read, object_paths_rows_read, lease_calls and blob ops/bytes, and check fault points meta.<method>.before / meta.<method>.after and blob.<method>.before / blob.<method>.after (FailAfter means the effect is applied and the call then fails). Leases emit a tracing span with target scion::lock. ObjectRepo: - put_blob/put_encoded/put_verified validate size (TOO_LARGE) and the decoded form of known kinds (Tree/Meta/Changeset/HistoryNode), and reject redacted ids (REDACTED). - Blobs always go to object storage. Tree/Meta/Derived objects at or below inline_threshold_bytes are stored inline, others as zstd frames at zstd_level. - Write order: bytes first (checkpoint upload.after_bytes), then the objects row (checkpoint upload.after_row); a live row therefore always implies durable bytes. - get/get_frame verify BLAKE3 of the decompressed content; on mismatch they record an integrity failure (Counters plus an ERROR log event 'integrity_alert') and return INTEGRITY. - Every read records object reads by kind and bytes. - stat/get use a stale read first and retry a miss with a strong read before NOT_FOUND. - LRU caches of decoded trees, metas and changesets serve immutable reads even while the metadata store is down. - quarantine_bytes copies to the quarantine key and then deletes the live key (checkpoint quarantine.after_copy). restore_bytes reverses it. purge_bytes deletes both keys. fsck walks every changeset and checks SPEC 4.5: digest of every live object, presence of root tree, meta, parents and delta blobs, delta == FileDiff(parent tree, root tree), generation and skip consistency, required eager rows, and ref targets exist. Quarantined or purged bytes are not violations. conformance::meta_store_suite and blob_store_suite are public async fns (always compiled, a test-support API). They cover every method contract, including the concurrency cases below, and run against MemMetaStore/MemBlobStore in this package's tests. P15 reuses them for CRDB/S3. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** Tests in crates/scion-store/tests: - meta_store_suite and blob_store_suite on Mem. - CAS: 1000 concurrent cas_ref calls at the same expected version give exactly one Committed; versions advance by exactly 1; ref_log has no gaps (19.5, store level). ABA: A->B->A, then a CAS expecting the first version fails. - LandedConflict rolls back the whole cas_ref. - 19.3: a blob-store fault before put leaves no objects row; FailAfter on put still leaves no row unless the row insert ran; a row never exists without bytes. Inline threshold at threshold-1, threshold and threshold+1 stores in the expected place and reads back identically. A stale-read miss is retried strongly (stale_lag) before NOT_FOUND. Orphan bytes (a put with no row) are invisible to stat/get. - 19.1: a blob id is independent of zstd level (1, 3, 19) and framing (a multi-frame encoding of the same content). - Corrupt stored bytes give INTEGRITY and integrity_failures +1 (16.1 class 7). - fail_txn_every leaves no partial state. - Redaction epoch increments atomically with the state changes. - Counters count exactly (tree reads include inline trees). - fsck detects each Violation kind on hand-corrupted state and passes on valid state.

##### P04: scion-config and scion-obs: TOML configs, structured logging, metrics, health

- **Size:** M  |  **Depends on:** P00
- **SPEC sections:** 6.1, 6.3, 6.4, 6.5, 15.1, 15.2, 15.3, 16.1 class 7 (integrity alert metric), 19.15, 19.17
- **Owns:** `crates/scion-config/src/**`; `crates/scion-config/tests/**`; `crates/scion-obs/src/**`; `crates/scion-obs/tests/**`
- **Deliverables:** Implement PLAN 2.11 and 2.12; lib.rs and Cargo.toml stay P00's. Config: - serde with #[serde(deny_unknown_fields)] everywhere, wrapped in serde_path_to_error so ConfigError::UnknownKey names the full dotted key (for example 'limits.max_blobs'). - '$NAME' string values are resolved from Env in a pre-pass over toml::Value, keeping the key path. An unresolved variable is ConfigError::UnresolvedVar naming the variable and the key. - Every default from SPEC 6.5 and PLAN 2.11 (including the implementation-defined (+) keys) applies when a key is omitted. - Durations use humantime ('12h', '7d'); millisecond keys are integers. - resolve_endpoint precedence: flag > SCION_ENDPOINT > working-copy config > user config. - for_tests constructors. Obs: - init_logging emits JSON (one event per line) or text. - RpcLogEvent::emit logs the REQUIRED 15.1 fields: request_id, rpc, tier, principal, repo, status, duration_ms, and when applicable changeset_id, ref, ref_version, land_attempt. - path_hash returns 16 hex characters of BLAKE3. Raw repository paths are never logged (PLAN 6.9 log access policy). - Metrics registers every SPEC 15.2 metric with the exact names and labels (scion_rpc_duration_seconds{rpc,tier,code}, scion_cache_requests_total{tier,kind,result}, scion_cache_bytes{tier}, scion_land_attempts{result}, scion_ref_cas_failures_total{ref}, scion_land_duration_seconds, scion_derive_duration_seconds{type,mode}, scion_derive_backlog{type}, scion_redaction_epoch{tier}, scion_redaction_lag_seconds{tier}, scion_policy_epoch_info{epoch}, scion_object_bytes_served_total{tier,encoding}, scion_upstream_requests_total{tier}), plus scion_integrity_failures_total{tier}. NAMES lists them all. - health_router serves /metrics (text exposition), /healthz (200 while alive) and /readyz (200 or 503 from Readiness::check). serve_admin binds it. - capture::LogCapture installs a process-global in-memory JSON layer for tests, with find_needles for log scans. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.15: an unknown key is rejected with the key name, at top level and in a nested section, for server, daemon and client configs. $VAR resolves. An unresolved $VAR errors, naming the variable and key. Every SPEC 6.5 default is applied when its key is omitted (one assertion per row of the 6.5 table, plus the (+) keys). The SPEC 6.1 example file parses with env vars supplied. 19.17 unit level: every 15.2 metric name appears in render() after one observation; /healthz returns 200; /readyz returns 503 then 200 as a toggled Readiness changes; RpcLogEvent produces all required fields as JSON keys; path_hash is stable and never equals the input; LogCapture.find_needles finds a planted secret.

##### P05: scion-policy: acl.toml parsing, evaluation, epoch, hot reload

- **Size:** M  |  **Depends on:** P00
- **SPEC sections:** 6.2, 12.4, 4.3.8, 16.1 class 9, 19.12 (policy semantics and reload)
- **Owns:** `crates/scion-policy/src/**`; `crates/scion-policy/tests/**`
- **Deliverables:** Implement PLAN 2.9. Parse with toml and serde (deny_unknown_fields; UnknownKey names the key). version must be 1. A '/' rule is REQUIRED (MissingRootRule). Groups: nested groups are resolved; cycles are a GroupCycle error listing the cycle; unknown groups are rejected. Principal forms: 'github:<login>' (compared case-insensitively), 'github-id:<n>' (matches Principal::GitHub{id: Some(n)}), 'service:<name>', 'group:<name>'. Prefixes are written with a leading '/' and normalized (no trailing '/'). Matching is component-wise, so '/secrets' matches secrets and secrets/x but not secrets2. For each path, the longest matching rule governs, independently for read and for write; rules never merge. read_governed_by_root reports whether the governing read rule is '/'. Refs: an exact name beats any pattern; among patterns (a single trailing '*' segment) the longest wins. Admin permissions: redact, purge, refs. Default deny. epoch = first 8 bytes, big-endian, of BLAKE3 over a canonical normalized serialization (sorted groups, rules and principals), so that equivalent files agree. PolicyHandle: arc-swap of Arc<Policy>; load() fails on an invalid or missing file. reload_now swaps atomically only when the new policy is valid. An invalid file keeps the last good policy, records last_error and logs an operator-visible ERROR. A watch channel publishes epoch changes. spawn_watcher uses notify plus a 1 s polling fallback. Tests use reload_now for determinism. allow_all_for_tests() grants everything to everyone. impl From<PolicyError> for ScionError (INVALID_ARGUMENT). Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.12 policy semantics: longest prefix wins; component-wise matching ('/secrets' vs '/secrets2'); independent read and write resolution; nested groups; cycle detection; the github-id: form; case-insensitive github logins; ref pattern precedence (exact > longest pattern) and default deny; admin permissions. Reload: a valid change is applied atomically and the epoch changes; an equivalent reformatted file gives the same epoch; an invalid change keeps the last good policy, surfaces last_error and leaves the epoch unchanged; an invalid or missing file at startup fails. A concurrency test: readers never observe a torn policy during repeated reloads.

##### P06: scion-auth: Ed25519 PASETO tokens, keyring rotation, constant-time secrets, GitHub client, device flow, FakeGitHub

- **Size:** M  |  **Depends on:** P00
- **SPEC sections:** 12.1, 12.2, 12.3, 17 (secrets never logged), 16.1 class 10, 19.11
- **Owns:** `crates/scion-auth/src/**`; `crates/scion-auth/tests/**`; `crates/scion-testkit/src/fake_github.rs`; `crates/scion-testkit/tests/fake_github_*.rs`
- **Deliverables:** Implement PLAN 2.10 and the FakeGitHub part of PLAN 2.21. Tokens: PASETO v4.public via pasetors (Ed25519). Claims sub, gid, iat, exp. kid goes in the footer as JSON {"kid":...}; kid = first 16 hex characters of BLAKE3(public key). Keyring::from_files reads PKCS#8 PEM Ed25519 keys. The first key signs and all keys verify. generate_for_tests and write_pem_files support harnesses. Verifier::verify returns UNAUTHENTICATED for malformed, unknown-kid, bad-signature, tampered and expired tokens (exp <= now). ct_eq wraps subtle::ConstantTimeEq and is the ONLY comparison primitive for secrets; under feature test-hooks it counts calls. ServicePrincipals::verify always computes SHA-256(secret) and calls ct_eq, comparing against a dummy digest for unknown names. Secret has redacting Debug/Display. GitHubClient::user calls GET {api_url}/user with 'Authorization: Bearer'. 401/403 maps to UNAUTHENTICATED; transport errors and 5xx map to UNAVAILABLE. device_flow::start/poll implement the GitHub device flow (POST {oauth_base}/login/device/code, POST {oauth_base}/login/oauth/access_token), handling authorization_pending, slow_down and expired_token. FakeGitHub (axum on 127.0.0.1:0) serves GET /user (tokens registered with add_user), the device-code endpoints (approve_device_code completes the flow) and a set_down switch returning 503. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.11 token items: a valid token verifies with the right sub and gid; expired, malformed, wrong-key, tampered-payload and tampered-footer tokens are rejected. Rotation: a token signed by key 2 verifies while key 2 is listed and fails once it is removed; new tokens are signed by the first key. Service secret: right succeeds, wrong fails, unknown name fails; ct_eq_calls increases on BOTH the right and the wrong path (the constant-time primitive is used on both). GitHubClient against FakeGitHub: valid token maps login and id; invalid token gives UNAUTHENTICATED; FakeGitHub down gives UNAVAILABLE. A device-flow round trip against FakeGitHub, including authorization_pending. Secret Debug output never contains the secret.

##### P07: scion-refmodel: independent reference model (19.0 oracle)

- **Size:** L  |  **Depends on:** P00
- **SPEC sections:** 4.1-4.4, 4.3.4 validity, 8.1 (validation outcomes), 8.2, 8.3, 8.4 (sequential land semantics), 9.2 (commit_graph, history TREESAME, blame), 11.2-11.3 (materialization, sparse scope), 13.2 (--all-versions), 13.5 (redacted content as empty)
- **Owns:** `crates/scion-refmodel/src/**`; `crates/scion-refmodel/tests/**`
- **Deliverables:** Implement PLAN 2.20 as a deliberately naive, single-threaded oracle. INDEPENDENCE RULES: the only third-party dependency is blake3 (and std). Do NOT read or copy any production crate under crates/ other than this one; work only from SPEC.md and docs/contracts/line-diff3-v1.md. check-layers.sh verifies that there are no scion dependencies. Contents: - Its own canonical encoder (enc) for trees (built recursively from the flat MTree), Meta and Changeset, plus id text. - apply_delta, which flattens, applies and rebuilds, returning the 8.1/18.2 subcode strings on invalid deltas (delete_absent, delete_not_file, noop_put, file_directory). - file_diff. - Its own Myers and diff3 written from the contract doc, using the simplest correct code, with no optimizations. - merge_trees implementing the full 8.3 table, both structural file_directory rules, the binary and merge_max_text_bytes rules, and redacted conflicts. - MRepo: commit graph (generation, skip pointers by naive walk), is_ancestor (full BFS), merge_bases (full ancestor-set intersection minus dominated), rebase per 8.2 (fast path, EMPTY), log_path with TREESAME semantics equivalent to the 9.2 history node-reuse rule (follow parents[0] when the content equals parents[0]'s, otherwise all parents where the path exists), blame per 9.2 (first parent, then subsequent parents, remaining lines to the commit; redacted content empty), all_versions (distinct contents ever held). - land_sequential. - materialize(sparse) per 11.2. O(n^2) algorithms are acceptable. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** Self-tests on hand examples taken from the SPEC tables: every 8.3 row and both structural rules; the 18.2 apply_delta subcodes; root changeset rules; generation and skip on a hand DAG including an octopus merge; a criss-cross DAG giving two merge bases; the TREESAME log on a merge that keeps the first parent's version; blame on a 3-commit example; all worked examples of docs/contracts/line-diff3-v1.md reproduced exactly; materialize with nested sparse prefixes. Property: file_diff(t, apply_delta(t, d)) == d for random t and random valid d (proptest_cases(5_000)). Cross-checks against production code are done later by P17 and other packages.

##### P08: scion-proto: conversions, error mapping, epoch trailer layer, ScionClient, RawCodec; testkit FakeUpstream and TestTls

- **Size:** L  |  **Depends on:** P00
- **SPEC sections:** 7.1, 7.2, 7.3-7.7 (message mapping), 7.8 (verbatim forwarding codec), 3.3, 17 (TLS), 19.7 (error mapping, forward compatibility)
- **Owns:** `crates/scion-proto/src/{convert.rs,status.rs,trailers.rs,client.rs,endpoint.rs,raw.rs}`; `crates/scion-proto/tests/**`; `crates/scion-testkit/src/{fake_upstream.rs,tls.rs}`; `crates/scion-testkit/tests/upstream_*.rs`
- **Deliverables:** Implement the Rust surface of PLAN 2.13 (the proto file and codegen are P00's and frozen), plus FakeUpstream and TestTls from PLAN 2.21. convert: From/TryFrom between v1 messages and scion-core/scion-object/scion-store types (ObjectId bytes, EntryInfo, DeltaEntry, MetaFields, ConflictReport, LandedEntry, RefRow/RefLogRow, Limits, ServerInfo pieces). A missing required field or a malformed id gives INVALID_ARGUMENT/missing_field or bad_id_text. Unknown fields are ignored (prost default). status: status_from_error sets the tonic code from ErrorCode::grpc_code and the message, and puts the ErrorDetail encoding (code string, retryable, message, payload oneof) into Status details. error_from_status inverts it; a Status without details maps from its gRPC code, defaulting to UNAVAILABLE. trailers: TRAILER_* constants, read_epochs/write_epochs. EpochTrailerLayer is a tower Layer over http Request/Response bodies. It appends scion-policy-epoch, scion-redaction-epoch and scion-tier to the HTTP/2 trailers frame of EVERY response: unary, server-streaming, client-streaming, error, and trailers-only responses (where tonic puts grpc-status in the headers, add the three keys to those headers). Epoch values are decimal u64. endpoint: Endpoint::parse accepts 'unix:///abs/path' and 'https://host:port'. client: ScionClient::connect uses tonic Channel over a UnixStream connector (hyper-util) or https with rustls (CA from TlsOptions, otherwise native roots). It installs a bearer-token interceptor and a response layer capturing the last epochs from trailers (and trailers-only headers), and keeps an atomic call counter (calls/reset_calls). raw: RawCodec is a tonic Codec whose Encode and Decode are both bytes::Bytes, for byte-level forwarding by the daemon. TestTls::generate creates an rcgen CA plus a server certificate for localhost and 127.0.0.1. FakeUpstream: an in-memory tonic server on 127.0.0.1:0 (TLS optional, via TestTls) implementing: ObjectService (GetServerInfo with configured public keys; GetObjects with per-item ok/not_found/denied/redacted, zstd or raw frames, offset/length ranges, access_class, kill_stream_after_bytes; HasObjects; ResolveRef); QueryService GetChangesets and Resolve; AuthService CheckAccess and WhoAmI; AdminService ListRedactions. Every other method returns UNIMPLEMENTED. The principal comes from the bearer token via the configured principal_of closure. The server is wrapped in EpochTrailerLayer using set_epochs values (tier server). It has set_down, corrupt, redact and deny knobs, and records every request (method, ids, principal). scion-proto tests must not dev-depend on scion-testkit (dependency rule PLAN 1.2); proto-level transport tests use rcgen directly. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.7: every ErrorCode round-trips through status_from_error/error_from_status with the correct tonic code and retryable flag, and every payload variant round-trips. A request with unknown fields (hand-encoded extra tag) decodes. Missing required fields in CreateChangesetRequest/MergeRequest give INVALID_ARGUMENT/missing_field. Property round trip for conversions. Transport tests (crates/scion-proto/tests, rcgen certificates, a minimal ObjectService implementation defined in the test): over TLS and over a Unix socket, ScionClient sees all three trailers on a unary success, a unary error, a server-streaming response, a client-streaming response and a trailers-only error, and last_epochs is updated; the call counter counts. RawCodec forwards a unary and a streaming call byte-identically through a test proxy. FakeUpstream tests (crates/scion-testkit/tests/upstream_*.rs): batch mixing ok/not_found/denied/redacted; range read; stream kill and resume by offset; requests are recorded.

##### P09: scion-testkit corpus: generators, shared-subtree repos, edge cases, vendored git bundles, linearizability checker

- **Size:** L  |  **Depends on:** P00
- **SPEC sections:** 19.0 (Corpus, History Recorder), 19.5 (linearizability checker), 19.14 (real git histories), 19.18 (equal-depth generated repos)
- **Owns:** `crates/scion-testkit/src/{corpus.rs,shared.rs,edge.rs,encode.rs,git.rs,lincheck.rs}`; `crates/scion-testkit/tests/corpus_*.rs`; `crates/scion-testkit/tests/lincheck_*.rs`; `corpus/git/**`
- **Deliverables:** Implement PLAN 2.21 except fake_upstream.rs and tls.rs (P08) and fake_github.rs (P06). corpus::generate(CorpusSpec): a seeded (rand_chacha) generator of GenRepo histories of structural MTree snapshots, with configurable file count, depth, fanout, history length, merge ratio, octopus ratio, binary ratio and symlink ratio, plus criss-cross shapes. Text edits are line-level, so merges are meaningful; output is deterministic per seed. shared::shared_subtree_repo(files, depth): a balanced tree of identical subtrees reaching 10^3, 10^5 or 10^7 logical files at the SAME depth (fanout chosen per size, depth fixed), perturbable along one path. encoded_objects() returns only the O(depth x fanout) distinct objects. edge::edge_cases(): empty root, deep path at the 4096-byte limit, 255-byte names, case-only-differing names, file<->dir flips, CRLF, NUL content (binary), executable flips, symlink retargets, delete-and-readd, unterminated last lines. encode::encode_tree uses scion-object to produce production-encoded trees and blobs from an MTree. It is exercised first by P17; this package must not run scion-object code in its own tests, because P01 is in the same wave. git: vendor at least two REAL open-source git histories as bundles under corpus/git/ (candidates: rust-lang/cfg-if and dtolnay/itoa, both MIT OR Apache-2.0 and small with merge commits). Verify license and size (each at most 5 MB, full history via `git bundle create <f> --all`), and add corpus/git/README.md plus LICENSE-<repo> files recording source URL, commit and license. Also build and commit synthetic edge bundles with the git CLI: an octopus merge, multiple roots, empty commits, mode 100664, symlinks, a gitlink, a merge keeping the first parent's version, and rename-free line edits for blame. Tests must then not require the git CLI; bundles(), unpack() (via gix) and GitFixture (which needs git and skips when it is absent) are provided. If network access is impossible, stop and report instead of substituting synthetic history for the real-repository requirement. lincheck: HistoryRecorder (thread-safe, monotonic timestamps) and check_ref_registers, a WGL linearizability search with memoization over a versioned-register model per ref. Read returns (version, target); Cas succeeds iff its expect equals the current version; Land succeeds with version+1. An Unknown completion may or may not have taken effect (crash or timeout). Histories are partitioned per ref; document the cap of about 200 operations per history. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** Generators are deterministic per seed and realize the requested shapes (octopus, multiple roots, criss-cross detected structurally). The shared-subtree repo has the same depth at 10^3, 10^5 and 10^7 files, and its distinct structural subtree count is O(depth x fanout) (computed structurally). Edge-case catalog sanity. lincheck accepts known-linearizable histories (sequential; concurrent with overlap; with Unknown completions) and rejects known violations (stale read after a completed CAS, two successful CAS at the same version, a version gap) on hand-built cases. Bundles exist, their README and licenses are present, and the size limits hold (no git CLI required).

##### P10: scion-cache: verified disk cache, coalescer, upstream limiter, ref/redaction/access caches

- **Size:** M  |  **Depends on:** P00
- **SPEC sections:** 10.2, 10.3, 10.4 (decision caches), 13.3 (daemon redaction set and staleness), 16.1 classes 6/7, 19.9
- **Owns:** `crates/scion-cache/src/**`; `crates/scion-cache/tests/**`
- **Deliverables:** Implement PLAN 2.15. DiskCache layout: <dir>/objects/<kind_letter>/<hex[0:2]>/<hex>, holding the payload as received (a zstd frame or raw bytes, with a 1-byte header distinguishing them). Insert writes a temp file in the same directory, verifies BLAKE3 of the decompressed content against the id (INTEGRITY, nothing cached, on mismatch), fsyncs and renames. open() deletes leftover temp files and rebuilds the size and access index by scanning. get verifies on read when verify_on_read is set; a mismatch deletes the entry and returns Ok(None) so the caller refetches. Approximate LRU by last access time: after an insert that pushes usage over max_bytes, evict down to low_watermark x max_bytes. usage() is exact. Coalescer is single-flight per key: concurrent callers share one future, errors are shared, and the entry is removed after completion. UpstreamLimiter: semaphore(max_concurrency) plus a bounded waiter count; overflow gives RESOURCE_EXHAUSTED immediately. RefCache: TTL-bound, and never returns a lower version than previously returned for a ref, even after TTL expiry or out-of-order upstream answers. LocalRedactions: id set, epoch, last refresh and is_stale. AccessCache: entries keyed by (principal, object) with TTL, plus a per-principal root-reader flag with TTL. Decisions never leak across principals, and on_policy_epoch(epoch) clears everything when the epoch differs. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.9 unit level: a corrupt insert is rejected and not cached; a bit flip in a cached file is detected on read (verify_on_read), deleted and get returns None; temp files are removed on open; a simulated crash after the temp write and before rename leaves no visible entry; 10000 concurrent get_or_fetch calls for one key run the fetch exactly once; a randomized insert/get workload keeps usage <= max_bytes at all times and evicted ids read back as None; ref TTL is honored and versions stay monotonic across TTL expiry and reordered observations; queue overflow gives RESOURCE_EXHAUSTED. AccessCache isolates principals and clears on an epoch change.

##### P11: scion-wc local layer: safe filesystem, stat-cache index, status engine, ignore, state, lock, credentials

- **Size:** L  |  **Depends on:** P00
- **SPEC sections:** 4.4 (path rules, case collisions), 6.4 (credentials), 11.1, 11.4, 11.5, 17 (checkout path safety, credentials 0600), 18.7, 19.10 (status, ignore, path safety), 19.11 (credentials)
- **Owns:** `crates/scion-wc/src/{fs_safe.rs,index.rs,status.rs,ignore.rs,state.rs,lock.rs,credentials.rs}`; `crates/scion-wc/tests/local_*.rs`
- **Deliverables:** Implement the corresponding modules of PLAN 2.17. SafeRoot uses rustix openat with O_NOFOLLOW for every path component (a pre-existing symlinked parent is refused, never followed). Writes go to a temp file plus renameat in the target directory, with the exec bit set for Mode::Executable; symlink() creates symlinks; nothing is ever written outside the root. is_case_insensitive probes the filesystem once. validate_entry_path enforces 4.4 and rejects '.scion' in any case. case_collisions uses ASCII case folding, so it is deterministic on any filesystem; checkout uses it whenever the filesystem is case-insensitive or WcOptions.force_case_insensitive is set. StatCache: (size, mtime_ns, ctime_ns, inode, mode) tuples plus written_at_ns, stored as a binary file .scion/index (versioned header) and saved atomically. is_racy means mtime >= written_at. status follows 18.7 over the TreeLookup trait: walk in-scope files, skip .scion and ignored files, skip inaccessible prefixes; hash files (symlinks hash their target) only when the stat tuple changed or the entry is racy; report modified/added/deleted/mode_changed; refresh clean entries; an unmodified redacted placeholder (index redacted flag plus placeholder bytes) is clean, and an edited placeholder is reported in redacted_dirty and modified; count hashed files. IgnoreMatcher uses ignore::gitignore (nested .gitignore files, negation, directory patterns, anchoring) with git semantics. state: WcState and PendingOperation as canonical TOML (sorted keys, deterministic), saved atomically under .scion/state. lock: fs4 advisory non-blocking lock on .scion/lock. credentials: write creates the file with mode 0600 (and its parent with 0700); read refuses a group- or world-readable file with PERMISSION_DENIED and a clear message. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.10 status: randomized edit sequences (create, modify, delete, chmod +x/-x, symlink swap, rename, truncate-and-write, atomic-rename save, edits within the same second as the index write, i.e. racy-clean) match a full-rehash oracle after every step (proptest_cases(500); #[ignore] full_ at 10^4). Clean status hashes only racy or stat-changed files. .gitignore semantics match `git check-ignore` on a corpus of at least 40 patterns (skipped when git is absent), and a pure table of expected results runs always. Path safety: '..', '.scion', '.SCION' and NUL paths are refused; writes through a pre-existing symlinked parent directory are refused and nothing is created outside the root. case_collisions reports collisions (deterministic on any fs; plus a real collision test when the tmp fs is case-insensitive, as on macOS APFS). State and index round-trip and are atomic (a crash simulated mid-save leaves the old file). The lock excludes a second holder. 19.11: the credentials file is created 0600; read refuses 0640 and 0604.

#### Wave 2

##### P12: scion-history: tree reading, apply_delta, FileDiff, CreateChangeset, bootstrap

- **Size:** L  |  **Depends on:** P01, P03, P07
- **SPEC sections:** 4.3.2, 4.3.4, 4.5, 5.3 step 2, 8.1, 9.2 (commit_graph columns, path_index rows), 13.6, 14 (parentless import_mode), 18.1, 18.2, 16.1 class 4, 19.2, 19.3, 19.18 (CreateChangeset gate)
- **Owns:** `crates/scion-history/src/{history.rs,tree.rs,apply.rs,diff.rs,create.rs,bootstrap.rs}`; `crates/scion-history/tests/create_*.rs`
- **Deliverables:** Implement the P12 part of PLAN 2.6 (History::new and accessors already exist). tree/lookup/changeset go through ObjectRepo caches. apply_delta follows 18.2 exactly: walk only the ancestors of delta paths, deepest first, applying deletes, then rebuilt children, then puts. Subcodes: delete_absent, delete_not_file, file_directory (ancestor file not deleted, or a directory not fully deleted), noop_put. An empty non-root directory is pruned; the root may become empty. Return the new root plus the new non-empty trees with their canonical bytes. file_diff prunes identical subtree ids and supports a scope filter. create_changeset validates in the exact 8.1 order with the first failure returned: - parents exist, are unique and within max_parents (TOO_LARGE); empty parents only when has_root_changeset() is false, or when import_mode is set and authz.can_import() (else parentless_not_allowed or PERMISSION_DENIED); - delta sorted/unique/limit/paths; - Put blobs live with matching size (missing_blob, size_mismatch); - REDACTED for quarantined or purged Put ids, while entries carried over unchanged need no check; - delete/put versus T1 (delete_absent, delete_not_file, noop_put, file_directory via apply_delta); - authz.can_write on every delta path (PERMISSION_DENIED); - Meta fields within max_message_bytes (TOO_LARGE). Then apply_delta, the expected_root_tree check (root_tree_mismatch), and Meta with creator = who.id_string() (a client never supplies creator). Persist per 5.3, passing checkpoints create.after_validate/after_trees/after_meta/after_derive/after_commit: 1. store new trees and the meta via ObjectRepo.put_encoded; 2. call the EagerHook with EagerInput; 3. compute generation/skip with compute_commit_graph; 4. build object_paths rows for every Put blob at its path and every new tree at its directory path (path_index); 5. one commit_changeset bundle. Identical requests return the same id and never error, both sequentially and concurrently. create_internal (used by rebase and merge) skips client-facing authz and accepts an existing meta id. bootstrap is idempotent: ensure the root changeset (empty root tree; Meta author 'scion <scion@localhost>', time 0/0, creator = config.bootstrap_principal, message 'root'; change_id = BLAKE3('scion-root-v1')[0..16]) and ref main at version 1 via cas_ref(Absent); an existing ref is fine. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.2: property test, for random trees T1 and random valid deltas: apply_delta's resulting tree, flattened, equals scion-refmodel apply_delta, and file_diff(T1, result) == delta (proptest_cases(2_000); #[ignore] full_ 10^5). Each subcode has a failing test: missing blob, size mismatch, noop put, delete absent, delete of a directory, file/dir without deletes (both directions), each path rule, each limit. Deleting the last file in a directory removes the directory; the root may become empty. File-to-directory and directory-to-file transitions with the required deletes succeed. A merge changeset's delta is validated against the first parent only. Identical CreateChangeset requests, sequential and 50-way concurrent, return one id and leave one row. expected_root_tree mismatch. creator is the server-set principal. 19.3: arm Crash at each create.* checkpoint and at meta.commit_changeset.before; afterwards fsck finds no violation and no visible changeset exists, and a retry returns the same id. 19.13: a Put of a redacted (quarantined) blob fails REDACTED; a changeset not touching a redacted file in the parent tree succeeds. 19.18 unit gate: on shared_subtree_repo at 10^3 and 10^5 (10^7 in #[ignore] full_), a 1-file CreateChangeset reads at most depth+2 trees and writes at most depth+1 trees (Counters deltas), independent of size. bootstrap is idempotent.

##### P13: scion-history: commit-graph queries (IsAncestor, MergeBase, first-parent, log iteration)

- **Size:** M  |  **Depends on:** P01, P03, P07
- **SPEC sections:** 9.2 commit_graph, 18.5, 8.4 (stack detection support), 11.8 (~n, ^k), 7.4 (Log without path), 19.6 (commit_graph)
- **Owns:** `crates/scion-history/src/graph.rs`; `crates/scion-history/tests/graph_*.rs`
- **Deliverables:** Implement the P13 part of PLAN 2.6 in graph.rs, as impl History blocks. Read ChangesetRows (generation, skip, parents decoded from the encoded changeset) through ObjectRepo/MetaStore; this package does not depend on create_changeset. is_ancestor follows 18.5 (generation-pruned DFS; a is an ancestor of itself). is_first_parent_ancestor uses skip pointers in O(log n). merge_bases returns ALL best common ancestors (common ancestors not dominated by another common ancestor), sorted by id, using generation-ordered search. nth_first_parent and nth_parent implement '~n' and '^k'. log_page walks newest-first by generation then id (deterministic), optionally first-parent only, with an opaque page token (an encoded frontier) giving stable paging. Tests build DAGs by inserting ChangesetRows directly into MemMetaStore, using P00's compute_commit_graph, and compare with scion-refmodel MRepo. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.6 commit_graph: generation and skip pointers equal the refmodel for random DAGs (including octopus merges, multiple roots and criss-cross). IsAncestor, is_first_parent_ancestor and MergeBase agree with the refmodel on random queries (10^4 by default; #[ignore] full_ 10^5). A criss-cross gives 2 bases. log_page paging is stable and complete (concatenated pages == the full walk). ~n and ^k edge cases (beyond root gives None).

##### P14: scion-derived framework: registry, engine, lazy iterative derivation, leases, generic query path

- **Size:** M  |  **Depends on:** P01, P03
- **SPEC sections:** 9.1, 5.2 (derived, derive_leases), 5.3 (eager rows), 13.5 (purge of content-bearing records), 19.6 (framework items)
- **Owns:** `crates/scion-derived/src/{registry.rs,engine.rs,ctx.rs,lease.rs}`; `crates/scion-derived/tests/framework_*.rs`
- **Deliverables:** Implement the P14 part of PLAN 2.7. Registry: register and look up by name and version; v1_default registers the commit_graph and path_index descriptors (computed by History), history and git_mapping as eager types (constructing scion_derived::history::HistoryType and git_mapping::GitMappingType, whose behaviour is implemented by P21), and blame as lazy (blame::BlameType, P24). latest_version; content_bearing_types. StoreDeriveCtx implements DeriveCtx over ObjectRepo: blob_or_empty returns empty for quarantined or purged blobs; meta returns None when redacted; put_object uses ObjectRepo.put_encoded. DerivedEngine: - impl EagerHook runs every registered eager type and concatenates their rows. required_eager lists eager (name, version) pairs for fsck. - get(ty, version, key) returns the stored value or derives lazily. The lazy algorithm uses an explicit work stack (NEVER recursion): find the nearest keys whose values are stored by walking deps(), then derive forward in dependency order, writing back every intermediate value with put_derived (ON CONFLICT DO NOTHING). - Leases (try_lease/release_lease) only deduplicate concurrent work. A derivation proceeds even if the lease is not acquired, after a short wait-and-recheck. - Version selection: None means the latest registered version; old versions remain readable. - get_stored. purge_content_for deletes records of content-bearing types whose source_object is the id. - Reports derive duration and backlog through the DeriveMetrics hook set via set_metrics (no scion-obs dependency). Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.6: two workers deriving the same key concurrently produce byte-identical values and exactly one row (test-only lazy type). A lazy derivation over a chain of 10^5 dependencies (default 10^4; #[ignore] full_ 10^5) completes on a thread with a 256 KiB stack (no recursion growth) and writes back every intermediate value. A version bump re-derives on query while the old version's values stay readable. A test-only type registered at runtime is queryable through DerivedEngine::get (the generic path) with no other change. Lease loss does not affect correctness. purge_content_for deletes only content-bearing records keyed by the id. The EagerHook output contains the rows of all registered test eager types.

##### P15: Gated real backends: scion-store-crdb (CockroachDB) and scion-store-s3 (S3/MinIO)

- **Size:** L  |  **Depends on:** P01, P03
- **SPEC sections:** 3.5, 5.1, 5.2, 5.3, 5.4, 16.1 classes 1/2, 19.3 (on real backends)
- **Owns:** `crates/scion-store-crdb/src/**`; `crates/scion-store-crdb/tests/**`; `crates/scion-store-s3/src/**`; `crates/scion-store-s3/tests/**`
- **Deliverables:** Implement PLAN 2.4 following docs/contracts/storage-semantics.md exactly. CRDB: embedded MIGRATIONS create every SPEC 5.2 table keyed by repo_id first, plus audit_log and the secondary indexes listed in the contract; migrate is idempotent. Each MetaStore method runs in one SERIALIZABLE transaction via deadpool-postgres and tokio-postgres, with a client-side retry loop on SQLSTATE 40001 (bounded, with jitter; when exhausted, return UNAVAILABLE/txn_retry). Inserts use ON CONFLICT DO NOTHING. cas_ref runs UPDATE ... WHERE name=$1 AND version=$2 (or INSERT for Absent and DELETE for delete), inserts the ref_log, inserts landed rows (a unique violation 23505 rolls back and returns LandedConflict with the existing row) and inserts the audit row, all in the same transaction. Consistency::Stale uses AS OF SYSTEM TIME follower_read_timestamp(). changesets_by_prefix uses a range scan on the id column. Connection errors map to UNAVAILABLE/store_unavailable. connect_fresh_for_tests creates a unique database. S3: object_store AmazonS3 with endpoint, path-style and allow_http for MinIO; get on a missing key returns None; copy and delete are idempotent; list uses prefix listing. Keys are prefixed with key_prefix (unique per test run). Every test is gated: when SCION_TEST_CRDB_URL or SCION_TEST_S3_ENDPOINT is unset it prints 'SKIP: ...' and passes. Test names start with external_. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** When the env vars are set: scion_store::conformance::meta_store_suite against CrdbMetaStore and blob_store_suite against S3BlobStore. Also: migration idempotency; 40001 retry exercised by two conflicting concurrent transactions; Stale reads may miss a just-committed row while Strong reads see it; 1000-way concurrent cas_ref yields exactly one winner. 19.3 on real backends: ObjectRepo over CRDB plus S3 with InstrumentedBlob faults (put fails) leaves no objects row; FailAfter on meta.insert_object leaves a durable row and bytes. When unset, every test prints SKIP and passes.

##### P16: scion-daemon: Unix-socket caching tier, local authorization, redaction tracking, verbatim forwarding

- **Size:** L  |  **Depends on:** P01, P04, P06, P08, P10
- **SPEC sections:** 3.3, 6.3, 7.3 (daemon semantics), 7.8, 10.1-10.5, 13.3 (daemon), 16.1 classes 1/6/7, 17 (socket permissions), 18.8 (daemon loop), 19.8, 19.9, 19.11 (daemon), 19.12 (daemon), 19.13 (daemon)
- **Owns:** `crates/scion-daemon/src/**`; `crates/scion-daemon/tests/**`
- **Deliverables:** Implement PLAN 2.16. Daemon::start binds a Unix socket (mode 0666; authorization rests only on tokens, never on socket identity) and serves a tonic Router. (a) ObjectService is implemented locally: - GetObjects: verify the bearer token locally with keys from upstream GetServerInfo (refreshed on unknown kid). Check LocalRedactions before serving any cached object (status redacted). Authorization per 10.4: root-class objects are served to principals confirmed as root readers via a positive CheckAccess on a root-class object, cached for access_cache_ttl_ms; restricted objects need a positive per-(principal, object) CheckAccess decision cached up to access_cache_ttl_ms; one principal's decision is never used for another. On a miss: Coalescer, then UpstreamLimiter, then upstream GetObjects (at most 3 attempts with jittered exponential backoff); verify the digest before caching and before replying. Items denied, not_found or redacted upstream are passed through unchanged. Payloads are byte-identical after decompression, and per-item statuses equal the server's. Range reads serve raw bytes. - HasObjects is forwarded, because it depends on upload records. - ResolveRef goes through the RefCache (TTL plus monotonic versions). - GetServerInfo is cached and refreshed periodically. (b) Every other gRPC path is forwarded verbatim through a RawCodec passthrough selected by path prefix. Request metadata (including authorization) and response trailers are passed through unmodified, so forwarded responses keep the server's scion-tier=server (PLAN 6.13). Epochs are observed from those trailers. (c) Locally served responses carry trailers from EpochTrailerLayer with tier=daemon and the daemon's latest known epochs. (d) A policy epoch observed in any upstream trailer that differs from the last one clears the AccessCache. (e) The redaction loop polls ListRedactions(since=local epoch) every redaction_poll_interval_ms and immediately on a higher trailer epoch. It applies the entries, deletes cached copies and marks refreshed. If is_stale(redaction_max_staleness_ms), it enters fail-closed mode: no cache serving, every read forwarded upstream; normal mode resumes after a successful refresh. (f) Upstream outage: cached objects keep serving; misses fail UNAVAILABLE; recovery needs no restart. (g) Metrics: cache_requests, cache_bytes, upstream_requests, redaction_epoch, redaction_lag, integrity_failures. RPC log events use tier daemon. (h) DirectUpstream records destinations for the network-policy test. (i) testing::InProcessDaemon. main.rs: load_daemon config, then start. Tests use scion_testkit::FakeUpstream (with principal_of built from a test Keyring Verifier) as upstream; real-server tier tests are P30's. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.9 daemon level: 10000 concurrent GetObjects for one missing id give exactly 1 upstream request; a corrupted upstream frame is rejected (INTEGRITY to the client, nothing cached); a bit flip in the cache is detected, deleted and refetched; temp files are cleaned on restart; a kill during insert leaves no partial entry and the cache stays valid (16.1 class 6); eviction keeps usage <= max and evicted objects are refetched transparently; ref TTL is honored and versions are monotonic across expiry and reordered upstream answers; upstream down gives cached hits served, misses UNAVAILABLE, and recovery without restart; queue overflow gives RESOURCE_EXHAUSTED. 19.8 basics: a batch mixing ok/not_found/denied/redacted is identical through the daemon and direct to FakeUpstream; QueryService/AuthService calls forwarded via RawCodec are byte-identical; only the configured upstream is contacted (DirectUpstream destinations). 19.11: the daemon rejects expired, malformed, wrong-key and tampered tokens. 19.12: a restricted object cached for principal A is not served to principal B without B's own positive decision; a policy epoch change in a trailer drops decisions. 19.13: a redaction is learned within one poll interval (ManualClock or short intervals) plus one request, and the cached copy is deleted; with polling blocked beyond max staleness the daemon stops serving from cache and forwards reads (fail closed). Trailers on daemon-served responses show tier=daemon.

##### P17: Oracle cross-validation: refmodel vs production encoding, Myers/diff3 vectors, FileDiff; testkit encoders

- **Size:** M  |  **Depends on:** P01, P02, P07, P09
- **SPEC sections:** 19.0 (Reference Model independence), 19.1 (golden vectors, id agreement), 19.4 (merge properties against the reference model), 4.3.4 (FileDiff)
- **Owns:** `crates/scion-conformance/tests/oracle_crosscheck.rs`; `crates/scion-conformance/tests/oracle_support/mod.rs`
- **Deliverables:** Write a cross-validation suite in scion-conformance that detects divergence between the independent oracle and production code early, before engines depend on it. (1) Every testdata/golden/objects vector: the refmodel enc produces identical bytes and id for trees, metas and changesets (build the refmodel value from the vector description). (2) Random valid trees (from MTree), metas and changesets: the scion-object id equals the refmodel id. (3) Every testdata/golden/diff vector is reproduced exactly by scion-refmodel::diff (myers edits and merge3 results/conflict ranges). (4) Differential: scion_merge::merge3 vs scion_refmodel::diff::merge3 on generated text triples (line-level random edits from a common base, including CRLF, unterminated last lines and identical edits); clean/conflict outcome and clean bytes must match. (5) scion_object::file_diff vs scion_refmodel::file_diff on random flat trees. (6) scion_testkit::encode_tree root ids equal the refmodel tree ids (first exercise of the testkit encoders). When a disagreement is found, do NOT edit P01/P02/P07 files. Minimize the failing case, record it, and report which side violates docs/contracts/line-diff3-v1.md or SPEC 4.1; the orchestrator routes the fix as an amendment. The package is done only when all checks pass, so fixes may be needed upstream; report them as amendment requests with the minimized case. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.1: golden vectors agree across both encoders; production id == refmodel id for proptest_cases(10_000) random objects (#[ignore] full_ 10^6). 19.4: merge3 differential at proptest_cases(10_000) (#[ignore] full_ 10^6); every golden diff vector is reproduced by the refmodel. FileDiff differential. Testkit encode_tree agrees with the refmodel.

##### P18: scion-wc remote layer: fetch walk with resume, checkout, goto, sparse, placeholders, redaction refresh, recovery dispatch, status integration

- **Size:** L  |  **Depends on:** P01, P07, P08, P09, P10, P11
- **SPEC sections:** 10.1, 11.2, 11.3, 11.5 (integration, zero server calls), 12.6 (inaccessible subtrees), 13.7 (placeholders, replacement of newly redacted clean files), 4.4 (case collisions), 17 (checkout path safety), 19.10, 19.13 (working copy), 19.18 (sparse checkout and status gates)
- **Owns:** `crates/scion-wc/src/{wc.rs,remote.rs,fetch.rs,tree_cache.rs,checkout.rs,goto.rs,sparse.rs,placeholder.rs,redaction_refresh.rs,recover.rs,diff.rs,testing.rs}`; `crates/scion-wc/tests/checkout_*.rs`; `crates/scion-wc/tests/goto_*.rs`; `crates/scion-wc/tests/sparse_*.rs`; `crates/scion-wc/tests/fetch_*.rs`
- **Deliverables:** Implement the P18 part of PLAN 2.17. remote: helpers over ScionClient (ResolveRef/Resolve, GetChangesets for root_tree, batched GetObjects with per-item status handling). fetch: breadth-first by depth, requesting only missing trees within the sparse scope in batches; skip out-of-scope subtrees; record denied subtrees as inaccessible; then stream missing in-scope blobs in batches. Every item is verified (BLAKE3 after decompression) before it is written to the local tree cache (.scion/cache via scion-cache DiskCache) or to the working copy. A broken stream is resumed by re-requesting incomplete objects with offset. checkout: validate every entry path (4.4, '.scion') and case collisions BEFORE any write. Write files atomically through SafeRoot (exec bits, symlinks); write redacted files as placeholder_bytes and mark the index entry redacted; initialize the index and WcState (base, tracking ref, sparse, inaccessible). Directories on the way to a scoped prefix are created, but their direct files are only materialized when in scope (11.2). goto: pruned diff of old and new trees within scope, applying only the differences. Dirty files the diff would touch abort unless force; other dirty files are preserved. sparse_add/sparse_remove: materialize or remove exactly the affected files; removing scope that contains dirty files fails unless force. Every mutating command first calls recover(), then refresh_redactions() (replace clean materialized copies of newly redacted blobs with placeholders, detected via GetObjects status redacted on the base tree's in-scope blobs; dirty files are reported and untouched), and journals a PendingOperation before touching files (fault points wc.goto.*, wc.sparse.*, wc.checkout.*). recover() completes or rolls back Checkout/Goto/SparseChange itself and dispatches Commit/Rebase to crate::commit::recover_commit and crate::rebase::recover_rebase (P28). status integration: a TreeLookup backed by the local tree cache, so status needs zero RPCs when trees are cached. diff(): working-copy changes. testing.rs (feature testing): reusable scenario functions taking a ScionClient, so P30 and P33 can re-run these suites against the real server and daemon. Tests use scion_testkit::FakeUpstream seeded via scion_testkit::encode_tree from generated corpora, including a malicious-tree mode. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.10 against FakeUpstream: a sparse checkout of directory D requests exactly the trees on the path to D plus the objects under D (asserted from FakeUpstream.requests()). Resumption: kill streams at random offsets; the resumed checkout re-fetches only incomplete objects and the working copy is byte-identical. The checkout result equals scion-refmodel materialize for random trees and sparse profiles (contents, exec bits, symlinks). Malicious trees ('..', '.scion', '.SCION', symlink traversal) are refused before any write, and pre-existing symlinked parents are not followed. Case collisions are reported, not overwritten (force_case_insensitive plus a real APFS test when available). goto touches only differing paths (mtime unchanged elsewhere), preserves unrelated dirty files and aborts on dirty files it would overwrite (force overrides). status makes zero server calls with a warm cache (ScionClient.calls()). sparse add/remove changes exactly the affected files and refuses dirty removal. Inaccessible (denied) subtrees are recorded and excluded from status. Crash injection at every wc.goto.*, wc.sparse.* and wc.checkout.* point leaves a pending_operation that the next command completes or rolls back. 19.13 working copy: checkout writes placeholders, status treats them as clean, newly redacted clean files are replaced on the next mutating command, dirty files are untouched and reported. 19.18: sparse checkout of a fixed directory fetches the same object count on shared_subtree_repo at 10^3 and 10^5 (10^7 #[ignore] full_); clean status reads zero objects upstream and hashes only stat-changed or racy files.

#### Wave 3

##### P19: scion-history: deterministic rebase and Merge(a,b) with the 8.3 tree-level rules

- **Size:** L  |  **Depends on:** P02, P07, P12, P13
- **SPEC sections:** 8.2, 8.3 (tree-level table, structural conflicts, binary/size/redacted rules), 13.6, 18.3, Appendix A (criss-cross), 19.4, 19.13 (merge with redacted content), 19.18 (Rebase gate)
- **Owns:** `crates/scion-history/src/{rebase.rs,merge.rs,conflict.rs}`; `crates/scion-history/tests/merge_*.rs`; `crates/scion-history/tests/rebase_*.rs`
- **Deliverables:** Implement History::rebase, History::merge and impl Rebaser for History (PLAN 2.6), following 18.3. rebase(C, X): - requires len(C.parents) == 1 (INVALID_ARGUMENT/multi_parent). - Fast path: X == P returns C itself. - B = tree(P), O = tree(X), T = tree(C). Examine exactly the paths in C.delta plus their ancestor directories in O (O(|delta| x depth)). - structural_conflict: T puts a file at p where O has a directory at p or a file at an ancestor of p, or T deletes p where O has a directory at p, gives file_directory. - Otherwise use scion_merge::classify. NeedContent: fetch b/o/t content via ObjectRepo. A redacted or purged side gives conflict redacted; is_binary on any side, or a size above limits.merge_max_text_bytes, gives binary; otherwise merge3, then Clean stores the merged blob via put_blob with mode from merge_mode, and Conflict becomes a content conflict with hunks. - Collect every conflict. When any exists, return MERGE_CONFLICT with ConflictReport{clean_entries for all non-conflicting delta paths, conflicts with base/ours/theirs EntryInfo and hunks}. - Otherwise delta = resolved entries differing from O; an empty delta gives EMPTY_CHANGE. Return create_internal(parents=[X], delta, MetaSource::Existing(C.meta), C.change_id). - No wall-clock value enters the result. Conflict markers are NEVER written to any stored object. merge(a, b): - merge_bases must be exactly one (else AMBIGUOUS_MERGE_BASE); zero bases is treated as the empty tree. - Compute FileDiff(base, a) and FileDiff(base, b) with identical-subtree pruning, and apply the same per-path rules with ours = a, theirs = b. - Write authorization: authz.can_write on every resulting delta path versus a. - Result: a new changeset with parents [a, b], new Meta from MetaFields, and the given change_id, via create_changeset's persistence path. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.4: a dedicated test for every 8.3 table row (o==b, t==b, o==t, add_add, modify_delete, symlink, binary, redacted, content clean, content conflict, mode merge) and both structural rules. merge_max_text_bytes boundary (equal passes, +1 gives binary). Rebase determinism: the same (C, X) gives the same id across runs, across two History instances over the same storage, and after recreating Storage handles. Fast path X == P returns C. EMPTY_CHANGE when the change is already in X. Zero blob reads when X changed only unrelated paths, and O(|delta| x depth) tree reads (Counters). Conflict payloads include clean_entries and per-conflict base/ours/theirs ids and hunks. A scan of every object created during the suite finds no '<<<<<<<' marker produced by the engine. Merge(a,b) with a unique base matches scion-refmodel merge_trees on generated corpora; criss-cross histories give AMBIGUOUS_MERGE_BASE. 19.13: a merge or rebase needing redacted content gives a redacted conflict. 19.18 unit: the Rebase gate on shared_subtree_repo at 10^3 and 10^5 (10^7 #[ignore] full_).

##### P20: scion-history: refs, UpdateRef, ref log, lock-free Land (with a fake-Rebaser seam)

- **Size:** L  |  **Depends on:** P07, P12, P13
- **SPEC sections:** 4.3.6, 4.3.7, 5.3 step 3, 8.4, 8.5, 18.4, 18.5 (stack detection), 16.1 class 5, 19.5 (library level), 19.18 (Land txn gate)
- **Owns:** `crates/scion-history/src/{refs.rs,land.rs}`; `crates/scion-history/tests/refs_*.rs`; `crates/scion-history/tests/land_*.rs`
- **Deliverables:** Implement the P20 part of PLAN 2.6. resolve_ref uses a strong read; at_version is served from the ref_log entry with that version (NOT_FOUND when absent). update_ref: the authz.can_update check; a version-checked cas_ref with a ref_log row in the same transaction; REF_VERSION_MISMATCH with a RefMismatch payload (current version and target). A non-fast-forward update (new target not a descendant of the old one, via is_ancestor) requires allow_non_fast_forward AND can_force, else NOT_FAST_FORWARD; forced updates write an audit row. create_ref/delete_ref use cas_ref(Absent) and Version(v) with an audit row. The caller (server) checks admin.refs. land/land_with implement the 8.4 state machine. Per attempt: 1. Resolving: a strong read of (tip, v), then authz = source.current() (policy re-read on EVERY attempt) and the can_update check. 2. Stack: the first-parent chain from head back to, and excluding, the first changeset that is an ancestor of tip. Use is_first_parent_ancestor when the tip's first-parent history is linear, otherwise fall back to is_ancestor. Every member must have exactly one parent (INVALID_ARGUMENT/merge_in_stack). 3. Checking: get_landed per member. Same source: skip and record. Different source: ALREADY_LANDED with payload. All skipped: return the recorded result (idempotent). 4. Rebasing: can_write on every delta path of each remaining member, then rebaser.rebase chained onto the previous result (checkpoint land.after_rebase). MERGE_CONFLICT fails the land; EMPTY_CHANGE fails unless allow_empty, in which case the member is skipped. 5. Committing: one cas_ref(Version(v)) with ref_log and landed rows (checkpoint land.after_commit). Every attempt reports to the configured LandObserver (Committed, Idempotent, CasMismatch, LandedConflict, Failed(code)) with its elapsed time. VersionMismatch or LandedConflict leads to full-jitter backoff between backoff_base and backoff_max (seedable rng; tokio::time::sleep) and a new attempt. After max_attempts: UNAVAILABLE/land_contention (retryable) with the ref unchanged. No lock or lease is ever taken (nothing emits a scion::lock span); a constant number of metadata transactions per attempt. land uses self as the Rebaser (impl from P19). Tests use land_with plus a fake Rebaser that re-applies C.delta onto `onto` via create_internal (conflict when apply_delta fails), so this package runs in parallel with P19. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.5 library level (fake Rebaser): ABA safety (A->B->A, then a CAS expecting the first version fails); fast-forward enforcement, with non-fast-forward requiring the flag and force; ref_log written atomically with gap-free versions; idempotency (arm FailAfter at meta.cas_ref.after, so land returns an error, then a retry returns the recorded result and no second ref version exists); an amended change id after landing gives ALREADY_LANDED; a partially landed stack skips lower members and lands upper ones; a stack containing a merge changeset is rejected; authorization is re-checked every attempt (a LandAuthorizerSource that revokes write after attempt 1, combined with a forced CAS mismatch, makes the land fail PERMISSION_DENIED); max_attempts exhaustion returns retryable UNAVAILABLE and leaves the ref unchanged; 50 concurrent non-overlapping lands all succeed with each change id landed once. No-locks: the Counters lease_calls delta is 0 and no scion::lock span is recorded on the land path. 19.18: a 1-file Land performs a constant number of meta_txns per attempt (equal at repository sizes 10^3 and 10^5).

##### P21: scion-derived types: history nodes (kind 0x10), path_index/commit_graph descriptors, git_mapping

- **Size:** M  |  **Depends on:** P07, P12, P14
- **SPEC sections:** 9.2 (history, path_index, git_mapping), 4.2 (derived kind 0x10), 14 (git_mapping via import_mode and Meta extension tag 2), 19.6 (history items), 19.18 (history node gate)
- **Owns:** `crates/scion-derived/src/{history_type.rs,path_index.rs,git_mapping.rs}`; `crates/scion-derived/tests/history_*.rs`; `crates/scion-derived/tests/git_mapping_*.rs`
- **Deliverables:** Implement the P21 part of PLAN 2.7. HistoryType (eager, not content-bearing): for each changeset, create history nodes only for paths whose content changed versus parents[0], meaning the delta paths and their ancestor directories (O(|delta| x depth) new nodes). Node reuse: when a path's content (blob id, or tree id for directories) equals its content in parents[0], reuse that parent's node. Otherwise create HistoryNode{kind, content, changeset, parents = nodes for that path in each parent where it exists (deduplicated, parent order), children (Dir: (name, node id) for every entry, reusing child nodes)}. Store each node via cx.put_object(Derived(0x10)). The per-changeset derived row is key = changeset id bytes, value = root node id. For the root changeset with an empty tree, the root node is a Dir with no children. node_for_path walks children from the root node; get_node decodes. path_index and commit_graph are descriptors only (rows and columns are produced by History::create_changeset). GitMappingType (eager, not content-bearing): only when EagerInput.import_mode is set and the Meta carries extension tag 2 (a 20-byte git commit id). It emits two rows: key 'g:'+40hex gives the changeset id, and key 'c:'+changeset id bytes gives the 40hex. Lookups changeset_for_git and git_for_changeset read rows via DerivedEngine::get_stored. Test the derive functions both through a mock DeriveCtx (an in-memory map) and end to end through History::create_changeset with DerivedEngine (Registry::v1_default) as the EagerHook. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.6 history: for every path in generated corpora (scion-testkit generate), walking node parents reproduces scion-refmodel MRepo::log_path's TREESAME history. The node reuse rule is verified on merges (a merge keeping the first parent's version reuses its node; a merge taking the second parent's version creates a node with both parents). Eager values exist for every visible changeset when Crash is armed at create.after_derive and at meta.commit_changeset.before (fsck with required_eager reports no violation). 19.18: a 1-file change creates at most depth+1 new history nodes (counted via objects_written of the derived kind) at 10^3 and 10^5. git_mapping: rows exist in both directions only for import_mode creations; a non-import changeset with tag 2 creates no mapping; a re-commit in import_mode fills in missing mapping rows.

##### P22: scion-redaction: Redact/Unredact/Purge lifecycle, quarantine moves, scheduled purge, backup-retention documentation

- **Size:** M  |  **Depends on:** P03, P14
- **SPEC sections:** 4.3.9, 13.1, 13.2, 13.5, 13.8, 17 (audit), 18.8 (server side)
- **Owns:** `crates/scion-redaction/src/**`; `crates/scion-redaction/tests/**`; `docs/operations/backup-retention.md`
- **Deliverables:** Implement PLAN 2.8 (authorization is checked by the server, not here). redact: every id must exist with kind Blob or Meta (else INVALID_ARGUMENT/not_redactable; unknown ids give NOT_FOUND). One MetaStore.redact transaction (epoch+1, rows stamped, objects quarantined, plus append_audit with actor and reason). Then move bytes to the quarantine key asynchronously (move_pending_bytes is idempotent and resumable after a crash at quarantine.after_copy; inline data stays in its row but is never served). Return {epoch, effective_by = now + propagation_bound}. unredact: only from Quarantined (else INVALID_ARGUMENT/not_quarantined); restore the bytes, then MetaStore.unredact (epoch+1) with audit. purge_now and run_due_purges (due_purges(now)): delete the live and quarantine keys and inline data, delete content-bearing derived records via DerivedEngine::purge_content_for, then mark_purged with audit. Purge is irreversible (checkpoint purge.after_bytes; a re-run completes it). spawn_background runs the mover and the purge scheduler every purge_interval. Write docs/operations/backup-retention.md: the backup retention policy (CockroachDB backups and object-store versioning/lifecycle at most 30 days; purged content persists in backups until they expire), the redaction runbook (redact, verify propagation, purge, verify backups expire) and verification steps (SPEC 13.8, 20.3). Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.13 lifecycle (library level over Storage::in_memory with ManualClock): redacting a blob changes no tree, changeset or ref id (ids captured before and after); the state goes Quarantined, then Purged after purge_after elapses and run_due_purges runs; unredact from quarantine restores reads byte-identically and bumps the epoch; unredact after purge fails not_quarantined; purge deletes the live and quarantine keys and inline data, deletes content-bearing derived records of a test content-bearing type keyed by the id, and keeps non-content-bearing records; a Meta object can be redacted; a Tree cannot (not_redactable); every action writes an audit row with actor and reason; a crash after the redact transaction but before the byte move is completed by move_pending_bytes; effective_by = commit time + propagation bound.

##### P23: scion-server core: app wiring, TLS, auth interceptor, trailers, error mapping, rate limit and load shedding, RPC logs/metrics, policy reload, RedactionSet, Login/WhoAmI/GetServerInfo, binary, InProcessServer

- **Size:** L  |  **Depends on:** P03, P04, P05, P06, P08, P12, P14, P15
- **SPEC sections:** 3.1, 6.1, 6.2 (reload), 7.1, 7.2, 7.3 (GetServerInfo), 7.7 (Login, WhoAmI), 10.5 (rate limits, load shedding), 12.1-12.3, 13.3 (server propagation), 15.1-15.3, 16.1 classes 9/10, 17 (TLS), 19.7, 19.11 (server), 19.12 (reload), 19.16 (GitHub down)
- **Owns:** `crates/scion-server/src/{main.rs,app.rs,context.rs,interceptor.rs,errors.rs,ratelimit.rs,logging.rs,reload.rs,tls.rs,redaction_set.rs,testing.rs}`; `crates/scion-server/src/handlers/{server_info.rs,auth.rs}`; `crates/scion-server/tests/core_*.rs`
- **Deliverables:** Implement the P23 part of PLAN 2.14. ServerApp::build wires the components in PLAN 1.4: ObjectRepo from config.objects, History (EagerHook = DerivedEngine with registry = deps.registry or Registry::v1_default; with_land_observer(MetricsLandObserver), defined in app.rs, which maps outcomes to scion_land_attempts{result}, scion_ref_cas_failures_total{ref} and scion_land_duration_seconds), Redactor, RedactionSet with spawn_refresher(propagation_bound) and observe() on epoch increments, ObjectAuthorizer, Verifier from the Keyring, ServicePrincipals and the policy watcher (when policy.reload is set). It also wires DerivedEngine::set_metrics with a Prometheus DeriveMetrics adapter (scion_derive_duration_seconds, scion_derive_backlog), and RedactionSet refreshes update scion_redaction_epoch{tier=server} and scion_redaction_lag_seconds. It calls History::bootstrap(), then handlers::admin::spawn_background(&ctx). ServerDeps.registry and ServerTestOpts.registry (frozen in PLAN 2.14) let core tests pass a registry without the history and git_mapping eager types, because those are implemented in parallel by P21. Layers, in order: request id, then a bearer interceptor verifying tokens with the clock (all RPCs except GetServerInfo and Login; inserts CallerCtx into request extensions; failure gives UNAUTHENTICATED), then load shedding (tower ConcurrencyLimit at rate_limit.max_inflight plus load_shed, giving RESOURCE_EXHAUSTED), then the governor per-principal limiter (RESOURCE_EXHAUSTED), then the RPC log and metrics layer (RpcLogEvent with tier server; scion_rpc_duration_seconds{rpc,tier,code}), then EpochTrailerLayer with AppContext as EpochSource. errors.rs maps ScionError to Status via scion-proto. Handlers: get_server_info (protocol_versions ['scion.v1'], repo, epochs, token public keys, limits, github_client_id, github_oauth_base_url); login (GitHub: GitHubClient.user, then Principal::GitHub{login, id} with gid in the token, invalid giving UNAUTHENTICATED and GitHub down giving UNAVAILABLE; service: ServicePrincipals.verify; returns token, principal, expires_at); who_am_i (principal, groups from policy, expires_at). reload.rs: on reload update the policy_epoch_info metric; an invalid file logs an ERROR and keeps the old policy. Readiness: metadata ping, policy loaded. The admin listener serves health_router. tls.rs: rustls server config from PEM. main.rs: load_server(config), build CrdbMetaStore and S3BlobStore, serve. testing::InProcessServer (feature testing): Storage::in_memory unless provided; temp dir holding acl.toml, Keyring PEM files, and TLS from TestTls when tls is set; ServerApp on 127.0.0.1:0; token_for issues tokens directly from the Keyring; set_policy writes the file and calls reload_now; kill() aborts the serving tasks; restart() builds a new ServerApp over the same Storage on the same address. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.11 server side: GitHub login against FakeGitHub (valid token gives principal github:<login> and the numeric id in the token's gid; invalid gives UNAUTHENTICATED); service login with right and wrong secret; the server rejects expired, malformed, wrong-key and tampered tokens on a protected RPC (WhoAmI). Key rotation through config (an old key still verifies while listed). 19.7: GetServerInfo and Login work without a token; every protected RPC without a token gives UNAUTHENTICATED (iterate the FILE_DESCRIPTOR_SET method list; stub handlers still sit behind the interceptor); all three trailers are present on success, error and streaming responses (WhoAmI, an UNAUTHENTICATED failure, GetObjects stub stream). 19.12: set_policy with a valid change changes the scion-policy-epoch trailer; an invalid file keeps the old epoch and the server keeps serving. 19.16 class 10: with FakeGitHub down, existing tokens still work and new GitHub logins fail UNAVAILABLE. Per-principal rate limit and load shedding return RESOURCE_EXHAUSTED (tiny limits). /readyz turns 503 when the MemMetaStore ping fails. Restart keeps data. RpcLogEvent fields are present for every RPC (LogCapture).

#### Wave 4

##### P24: scion-derived blame (lazy), Log(path), all-versions; Log and Blame RPC handlers

- **Size:** M  |  **Depends on:** P02, P07, P13, P21, P23
- **SPEC sections:** 9.2 (blame, history TREESAME), 7.4 (Log, Blame), 12.6 (Log/Blame filtering), 13.2 (--all-versions), 13.4 (redacted Meta in Log), 13.5, 19.6 (blame, Log(path), version bump), 19.12 (Log/Blame filtering), 19.13 (all-versions, Meta rendering)
- **Owns:** `crates/scion-derived/src/{blame.rs,log.rs,versions.rs}`; `crates/scion-derived/tests/blame_*.rs`; `crates/scion-derived/tests/log_*.rs`; `crates/scion-server/src/handlers/history_query.rs`; `crates/scion-server/tests/history_query_*.rs`
- **Deliverables:** Implement the P24 part of PLAN 2.7 and the Log/Blame handlers of PLAN 2.14. BlameType (lazy, not content-bearing, version configurable through with_version). The key is a file history node id. deps() are the blame keys of the node's parent nodes. derive follows 9.2: diff the node content against parents[0]'s content with scion_merge::line_origins and attribute matching lines from that parent's blame, then do the same with subsequent parents for the remaining lines; remaining lines belong to node.changeset. Redacted content is treated as empty (cx.blob_or_empty). Values are encoded with encode_ranges as a compact canonical list of ranges, adjacent equal-origin lines merged. blame(e, cs, path, version) resolves node_for_path (a missing or non-file path gives NOT_FOUND) and then calls e.get('blame', version, node id). log_path walks history node parents (first_parent_only follows parents[0]), returning node.changeset in deterministic generation-descending order, with a page token. versions::all_versions walks every reachable node for the path and returns distinct content blob ids. handlers/history_query.rs: - log: no path means History::log_page; with a path, log_path. - Read checks: Log with a path requires read on the path (PERMISSION_DENIED); Log without a path returns each changeset's GetChangesets-style item, where a delta path the caller cannot read produces a filtered view with those entries removed. - MetaView is redacted=true with only meta_id when the Meta is quarantined or purged (clients render '<redacted>'). - blame requires read on the path and returns ranges. - Errors map via scion-proto; the CallerCtx comes from context::caller. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.6: blame equals scion-refmodel MRepo::blame for every file at several changesets in a generated corpus (with merges). Log(path) via log_path equals the refmodel's TREESAME history for every path in the corpus. Lazy blame over a 10^4-commit single-file history completes iteratively (#[ignore] full_ 10^5). Version bump: registering BlameType::with_version(2) makes Blame re-derive on query through the unchanged RPC while version-1 values stay readable via DerivedEngine::get. Blame of a file with redacted history content is deterministic (redacted treated as empty). 19.13: all_versions finds every distinct blob a path ever held (compared with refmodel all_versions); Log renders redacted Meta as redacted=true with ids unchanged. RPC tests over InProcessServer: Log and Blame success; NOT_FOUND; PERMISSION_DENIED for a restricted path; Log without a path omits unreadable delta entries (19.12); unauthenticated calls fail; trailers are present.

##### P25: scion-server object, upload, access and admin services: retroactive object authorization, redaction RPCs, audit, background jobs

- **Size:** L  |  **Depends on:** P12, P21, P22, P23
- **SPEC sections:** 7.3 (GetObjects, HasObjects, ResolveRef), 7.5 (UploadObjects), 7.7 (CheckAccess, Redact, Unredact, PurgeNow, ListRedactions), 10.4 (CheckAccess semantics for daemons), 12.5, 13.3, 13.4, 13.6, 16.1 class 7, 17 (audit), 18.6, 18.8, 19.7, 19.12, 19.13, 19.18 (object authorization gate)
- **Owns:** `crates/scion-server/src/handlers/{object.rs,upload.rs,access.rs,admin.rs}`; `crates/scion-server/src/authz/object.rs`; `crates/scion-server/src/jobs.rs`; `crates/scion-server/tests/object_*.rs`; `crates/scion-server/tests/admin_*.rs`
- **Deliverables:** Implement the P25 handlers of PLAN 2.14 and ObjectAuthorizer::can_read per 18.6. can_read: first RedactionSet.contains gives Redacted. Then check the (principal, id, policy_epoch) decision cache (bounded LRU). Then read object_paths(id) rows for THIS id only, stopping at the first path the policy lets the principal read: Allow(Root) when that path's read rule is '/', otherwise Allow(Restricted) (if any readable row is root-governed the class is Root). No rows, or none readable, gives Deny. Trees: a tree's object_paths rows come from path_index (new trees at their directory path), so root and subdirectory trees authorize the same way. The root tree is recorded at the empty path, governed by the '/' rule. get_objects streams per item: ok (StoredFrame::Zstd passed through unmodified as ZSTD, inline data as RAW; total_size; access_class; final), not_found, denied, redacted. Range reads (offset/length) return RAW decompressed slices. A missing or denied item never fails the batch. Server-side corruption gives an INTEGRITY item failure with metric and alert. Bytes served increment scion_object_bytes_served_total{tier=server,encoding}. has_objects: true only when the object exists and is readable, or the caller uploaded it (has_upload); otherwise false. resolve_ref: History::resolve_ref with at_version (strong). upload_objects (client stream): per item, verify the kind and the digest of the data (raw content), validate the decoded form for structured kinds, reject redacted ids (REDACTED), put via ObjectRepo, record_upload(principal). A redundant upload of an existing object is accepted and deduplicated with no leak (status ok without revealing prior existence). check_access: verify subject_token with the Verifier, then can_read for each id, returning allowed and access_class (used by daemons). Admin RPCs check Policy::can_admin (redact and unredact need Redact; purge_now needs Purge) and call the Redactor with actor = the caller's principal. list_redactions needs a valid token only (daemons poll it). Every admin action is audited by the Redactor. jobs.rs plus admin::spawn_background start Redactor::spawn_background (quarantine mover and purge scheduler). Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.7 for GetObjects, HasObjects, ResolveRef, UploadObjects, CheckAccess, Redact, Unredact, PurgeNow and ListRedactions: success, each error code each can return, and authentication failure. A batch mixing ok/not_found/denied/redacted returns each status correctly. 19.12: retroactivity (after set_policy adds a restriction, objects appearing only under the new prefix become unreadable immediately with no reindex, while objects also present at an unrestricted path remain readable); the HasObjects existence probe (false for an existing object the caller cannot read and did not upload; true after the caller uploads it; a duplicate upload succeeds without leaking); unreferenced uploaded objects are not readable via GetObjects by anyone; admin RPCs require their permissions. 19.13 server: after Redact, GetObjects returns redacted no later than effective_by (ManualClock plus the RedactionSet refresher bound); re-upload of a redacted id fails REDACTED; ListRedactions returns entries and current_epoch; the audit log records actor and reason; unredact restores reads byte-identically. 16.1 class 7: corrupting stored bytes yields an INTEGRITY item and scion_integrity_failures_total increments. 19.18: object authorization reads object_paths rows only for the requested object (Counters object_paths_rows_read equals that object's row count, independent of repository size at 10^3 and 10^5).

##### P26: scion-server write and ref services: CreateChangeset, Rebase, Merge, Land, UpdateRef, CreateRef, DeleteRef, GetRefLog, policy authorizer adapters

- **Size:** L  |  **Depends on:** P19, P20, P21, P23
- **SPEC sections:** 7.5 (CreateChangeset, Rebase, Merge, Land), 7.6, 8.1, 8.4, 8.5, 12.4, 14 (import_mode authorization), 15.2 (land metrics), 17 (audit of ref admin and forced updates), 19.2 (RPC items), 19.4 (RPC determinism), 19.5 (RPC items), 19.7, 19.12 (write checks)
- **Owns:** `crates/scion-server/src/handlers/{write.rs,refs.rs}`; `crates/scion-server/src/authz/policy_adapters.rs`; `crates/scion-server/tests/write_*.rs`; `crates/scion-server/tests/refs_*.rs`
- **Deliverables:** Implement the P26 handlers of PLAN 2.14 and the policy adapters. PolicyWriteAuthz(Arc<Policy>, who) implements WriteAuthorizer: can_write = Policy::can_write; can_import = can_admin(Refs) && can_write('/'). PolicyLandAuthzSource(handle, who) implements LandAuthorizerSource: current() snapshots PolicyHandle::current() on every call, so policy is re-read on each land attempt. create_changeset converts the request (a missing meta_fields or change_id gives INVALID_ARGUMENT/missing_field; any client-supplied creator is impossible because MetaFields has no creator) and calls History::create_changeset with PolicyWriteAuthz. rebase calls History::rebase. Rebasing someone else's changeset is allowed for any repository reader: rebase needs read on '/' only, because results are unnamed changesets and landing checks write. merge calls History::merge with PolicyWriteAuthz. land calls History::land with PolicyLandAuthzSource. The land metrics come from the LandObserver that P23 wires into History. This handler logs land_attempt, ref, ref_version and changeset_id on the RPC event. update_ref calls History::update_ref with the current policy snapshot as LandAuthorizer. create_ref and delete_ref require can_admin(Refs) and pass the reason into the audit. get_ref_log returns entries. MERGE_CONFLICT, REF_VERSION_MISMATCH and ALREADY_LANDED payloads are encoded through scion-proto. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.2 via RPC: creator is server-set (a principal X creates and Meta.creator == X; MetaFields has no creator field); expected_root_tree mismatch is rejected; import_mode without admin.refs gives PERMISSION_DENIED; import_mode allows a second parentless changeset. 19.5 via RPC: 1000 concurrent UpdateRef calls at the same expected version give exactly one success, versions increase by exactly 1, and the ref log has no gaps; ABA; NOT_FAST_FORWARD without the flag or without force, success with both; Land happy path; Land idempotent retry after FailAfter at meta.cas_ref.after returns the recorded result. 19.4: Rebase via RPC gives the same id across InProcessServer.restart(). 19.12 write checks: CreateChangeset and Land reject unwritable paths; ref update and force are enforced per ref pattern; CreateRef and DeleteRef require admin.refs. 19.7: every write and ref RPC has success, each listed error (MERGE_CONFLICT payload with clean_entries, EMPTY_CHANGE, AMBIGUOUS_MERGE_BASE, REF_VERSION_MISMATCH payload, NOT_FAST_FORWARD, ALREADY_LANDED payload, INVALID_ARGUMENT subcodes, TOO_LARGE, REDACTED, PERMISSION_DENIED) and authentication failure. The land metrics move.

##### P27: scion-server query service: GetChangesets, ReadPath, ListTree, Diff, IsAncestor, MergeBase, Resolve, GetDerived; history filtering

- **Size:** L  |  **Depends on:** P12, P13, P14, P21, P23
- **SPEC sections:** 7.4 (except Log/Blame), 9.1 (generic derived query path), 11.8 (revision resolution), 12.6, 13.4, 4.2 (prefix resolution with candidates), 19.1 (ambiguous prefix), 19.6 (generic path), 19.7, 19.12 (history filtering), 19.13 (ReadPath REDACTED)
- **Owns:** `crates/scion-server/src/handlers/{query.rs,derived.rs}`; `crates/scion-server/src/revision.rs`; `crates/scion-server/tests/query_*.rs`
- **Deliverables:** Implement the P27 handlers of PLAN 2.14. Every query requires repository read (read on '/') unless noted. get_changesets: per item ok/not_found. Return the full canonical encoding only when every delta path is readable; otherwise a filtered_view with the unreadable delta entries removed and filtered = true (12.6). Include the generation. read_path: read on the path required; resolve the entry through History::lookup; stream the content for files; REDACTED for a redacted blob; NOT_FOUND for a missing path; a directory returns the entry with no content. list_tree: read on the directory; entries include restricted subdirectory names and ids (12.6 limitation); the tree id is returned. diff: History::file_diff between a and b with optional path scope; entries the caller cannot read are omitted; include_content_hunks uses scion_merge::unified_hunks for text, with binary flagged; streamed. is_ancestor and merge_base map to History. resolve (revision.rs): parse_revision, then the base: - Id: must exist. - IdPrefix or a bare hex Name (8 or more hex digits): changesets_by_prefix, with zero giving NOT_FOUND and more than one giving INVALID_ARGUMENT/ambiguous_prefix with a Candidates payload. For a bare Name, try an exact ref name first. - Ref and RefAt: History::resolve_ref. - Git: git_mapping::changeset_for_git. Then apply the ~n/^k suffixes through History. get_derived: DerivedEngine::get(type, version, key) for registered types; an unknown type gives NOT_FOUND. Content-bearing types require read on '/', and so do non-content types. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.12 history filtering: GetChangesets returns a filtered view for a principal lacking read on one delta path, and the full encoding when every path is readable; Diff omits restricted entries; ReadPath and ListTree on restricted paths give PERMISSION_DENIED; restricted subdirectory names are visible in a readable parent listing. 19.13: ReadPath on a redacted blob gives REDACTED. 19.1: an ambiguous short prefix (two changesets crafted to share an 8-hex prefix by brute-force search over change_ids) is rejected with both candidates; full ids, c1-prefixes, refs, ref@version, git:<sha> (via an import_mode changeset) and ~n/^k resolve correctly. 19.6: a test-only derived type registered through ServerTestOpts.registry is queryable through GetDerived with no client change; a version parameter selects old values. 19.7: every query RPC has success, NOT_FOUND, INVALID_ARGUMENT, PERMISSION_DENIED and UNAUTHENTICATED cases; trailers are present on the streaming Diff and ReadPath.

#### Wave 5

##### P28: scion-wc write layer: commit, amend, rebase with conflict materialization, resolve, abort, land, commit/rebase recovery

- **Size:** L  |  **Depends on:** P18, P25, P26, P27
- **SPEC sections:** 11.6, 11.7, 11.3 (pending-operation recovery), 13.7 (editing placeholders), 16.1 class 8, 19.10 (commit, amend, conflict flow, crash recovery, inaccessible paths)
- **Owns:** `crates/scion-wc/src/{commit.rs,rebase.rs,resolve.rs,land.rs,testing_write.rs}`; `crates/scion-wc/tests/commit_*.rs`; `crates/scion-wc/tests/rebase_*.rs`; `crates/scion-wc/tests/land_*.rs`; `crates/scion-wc/tests/recover_*.rs`
- **Deliverables:** Implement the P28 methods of WorkingCopy (PLAN 2.17) plus crate::commit::recover_commit and crate::rebase::recover_rebase, which P18's recover() dispatches to. commit: run status within scope (paths restricts it); refuse paths outside scope or inaccessible. Compute the delta versus base (Put with mode, blob id and size; Delete), sorted. HasObjects, then UploadObjects only for missing blobs. Journal PendingOperation::Commit (fault points wc.commit.after_journal, after_upload, after_create), then CreateChangeset(parents = [base], new ChangeId::random(), MetaFields with author and timestamp from the clock and the local tz). Move base to the result, refresh the index and clear pending. amend: CreateChangeset(parents = base.parents, same change_id), reusing the original Meta fields unless a new message is given. rebase(onto): resolve onto, compute the local stack (the first-parent chain from base back to the first ancestor of onto, via IsAncestor), journal PendingOperation::Rebase, and call server Rebase per member, chaining. On MERGE_CONFLICT: goto onto's merged state (the previous rebased member), write clean_entries, then for each conflict: - text (content kind): write the file with render_markers(base, ours, theirs) using the 11.7 markers; - other kinds: write the ours version (or nothing when absent) and save every side under .scion/conflicts/<path>.{base,ours,theirs}. Record conflicts in pending. resolve(path, force) refuses while marker lines remain, unless force. commit while rebasing requires every conflict resolved, creates the changeset with the original member's change_id, author and message (parents = [current onto]), then continues with the remaining stack. rebase_abort returns to the original changeset (goto original) and clears pending. land: server Land(ref, base); on success base becomes the landed result (resolved via the returned target, the stack member's result), and tracking is updated. recover_commit: if the changeset was created (detectable by re-issuing the deterministic CreateChangeset, which is idempotent), finish by moving base; otherwise roll back by clearing pending, leaving the files untouched. recover_rebase resumes or rolls back to a consistent state. testing_write.rs (feature testing) exports scenario functions for P30 and P33. Tests run against InProcessServer (real server) directly. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.10: commit uploads only missing blobs (UploadObjects item count equals the new blobs) and sends a delta equal to scion-refmodel file_diff(base tree, working tree). amend preserves change_id. Commits cannot include out-of-scope or inaccessible paths. Full conflict flow: markers written for text conflicts, side files for binary/symlink/modify_delete, resolve refuses remaining markers (force overrides), continuing the commit preserves change_id, author and message, and --abort restores the original exactly. Crash injection at every wc.commit.* and wc.rebase.* fault point leaves a recoverable pending_operation that the next command completes or rolls back, after which status is consistent and the server state is valid. Land moves base to the landed result. Editing a redacted placeholder and committing creates new content normally (13.7).

##### P29: scion-import: one-way git importer with fidelity and incremental re-import

- **Size:** L  |  **Depends on:** P09, P24, P25, P26, P27
- **SPEC sections:** 14, 9.2 (git_mapping), 4.3.3 (extension tag 2), 4.3.4 (importer parentless changesets), 19.14, 19.6 (blame vs git blame)
- **Owns:** `crates/scion-import/src/**`; `crates/scion-import/tests/**`
- **Deliverables:** Implement PLAN 2.19 using gix (pure Rust; no libgit2). CLI: `scion-import --git <repo> --ref <git-ref> --target-ref main [--skip-submodules] [--endpoint ..]`. The token comes from SCION_TOKEN or service login using SCION_SERVICE_NAME/SCION_SERVICE_SECRET. Walk commits reachable from the git ref in topological order, parents first. For each commit: - Skip it if Resolve('git:<sha>') already resolves (incremental and idempotent). - Upload blob contents not yet present (HasObjects, then UploadObjects in batches up to batch_bytes). - delta = FileDiff(first parent's git tree, commit tree) using map_mode: 100644/100664 map to File, 100755 to Executable, 120000 to Symlink. Empty git trees are dropped naturally because Scion has no empty directories. Gitlinks (160000) fail the import unless skip_submodules is set; when it is, they are omitted and recorded in Meta extension tag 4 (list<(path, 20-byte sha)>). - Meta: author and committer bytes 'Name <email>', author_time/committer_time seconds plus tz offset in minutes, message bytes verbatim, extension tag 2 = the 20-byte git commit id. - change_id = BLAKE3('scion-import-v1' || 20-byte id)[0..16]. - CreateChangeset with import_mode = true and parents = the mapped Scion ids of all git parents (octopus merges keep every parent; parentless roots are allowed). The server derives git_mapping eagerly. After the walk: CreateRef(target, tip) when absent, else UpdateRef fast-forward with expected_version. ImportReport. testing.rs (feature testing) exposes fidelity helpers. Tests use InProcessServer with a service principal that has write on '/' and admin.refs, plus the vendored corpus/git bundles (P09). Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.14: differential fidelity over every commit of both real bundles and every synthetic edge bundle. The Scion tree's file set, modes and contents (ListTree/ReadPath, or GetObjects plus tree walks) equal the git tree read via gix, and via `git ls-tree -r` too when the git CLI is present; skipped gitlinks and empty trees are the documented exclusions. Meta preserves author, committer, both timestamps and zones, and message byte-for-byte. git_mapping resolves in both directions for every commit (Resolve('git:<sha>') and GetDerived git_mapping 'c:' key). Re-import is idempotent (MemMetaStore table_counts unchanged). Incremental import after adding commits imports only the new ones and fast-forwards the ref. Octopus merges, root commits, multiple roots, empty commits, mode 100664 and symlinks import correctly. A gitlink fails without --skip-submodules and is recorded in Meta extension 4 with it. 19.6: Blame RPC output equals `git blame --porcelain` line origins (mapped through git_mapping) for every file without renames in the imported real corpora (skipped when git is absent); exceptions are documented in tests/BLAME_EXCEPTIONS.md.

##### P30: scion-conformance harness and suites A: TestCluster, RPC contract matrix, tier conformance, auth end to end, ACL matrix, observability

- **Size:** L  |  **Depends on:** P16, P24, P25, P26, P27
- **SPEC sections:** 7.1-7.8, 10.4, 12.1-12.6, 15.1-15.3, 19.0, 19.7, 19.8, 19.11 (end to end), 19.12 (matrix), 19.17
- **Owns:** `crates/scion-conformance/src/{cluster.rs,invariants.rs,seed.rs,support.rs}`; `crates/scion-conformance/tests/{rpc_contracts.rs,tier.rs,auth_e2e.rs,access_matrix.rs,observability.rs}`
- **Deliverables:** Implement PLAN 2.22 (lib.rs is P00's and declares the modules). TestCluster::start: - Backend::Memory gives Storage::in_memory. Backend::External gives CrdbMetaStore::connect_fresh_for_tests plus S3BlobStore::test_config wrapped by Storage::new. - N InProcessServers share that Storage (servers > 1 for the multi-server suites). - An optional InProcessDaemon in front of servers[0]. - A FakeGitHub. - A LogCapture installed. - A default ACL with groups eng/admins/security, '/' readable by eng, and a /secrets restriction. client(via, who) returns a ScionClient authenticated as who, directly or through the daemon. seed(GenRepo) uploads blobs and creates changesets through RPCs, returning ids. counters() snapshots. invariants::check_16_2 runs fsck, checks ref-log provenance (every ref version equals a recorded successful update) and verifies digests of every object served in the run. Suites run with Backend::from_env(): Memory always, External when the env vars are set. rpc_contracts.rs: a table-driven matrix that enumerates every RPC from v1::FILE_DESCRIPTOR_SET, so an RPC without a case fails the test. For each RPC: success, each error code listed for it in SPEC 7.2-7.7, and authentication failure. ErrorDetail.retryable matches 7.2 for every code observed. All three trailers are on every response from both tiers. Unknown request fields are ignored; missing required fields give INVALID_ARGUMENT. tier.rs: ONE read-RPC suite (GetServerInfo, GetObjects including batches, ranges and mixed statuses, HasObjects, ResolveRef) run unchanged with Via::Server and Via::Daemon; per-item results and decompressed payloads are byte-identical for the same principal and epochs. Write, query, ref, auth and admin RPCs through the daemon behave identically to direct calls. Network-policy test: DirectUpstream destinations contain only the configured upstream. Re-run the P18 scion_wc::testing checkout scenarios against the real stack via the daemon. auth_e2e.rs: GitHub and service login through the daemon; token rejection at both tiers; rotation. access_matrix.rs: every read and query RPC x {allowed, denied at '/', denied at a subdirectory} x {server, daemon} returns the specified result. observability.rs: after running a representative workload, every RPC log event has the required fields; LogCapture.find_needles finds no token, service secret, GitHub token, file content or raw restricted path (PLAN 6.9); every 15.2 metric is exported and moves in its scenario; /healthz and /readyz reflect dependency state (MemBlobStore.set_down and the MemMetaStore fault point make readyz 503). Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.7 (complete matrix, descriptor-driven), 19.8 (tier conformance including the network policy), 19.11 end to end through the daemon, 19.12 matrix, 19.17 (log fields, log scan, metrics move, health), all on the Memory backend and additionally on External when SCION_TEST_CRDB_URL and SCION_TEST_S3_ENDPOINT are set (test names prefixed external_ for the External variant).

#### Wave 6

##### P31: scion-cli: all core commands, --json with versioned published schemas, device-flow login, redact tooling

- **Size:** L  |  **Depends on:** P28, P30
- **SPEC sections:** 11.8, 6.4, 12.2, 13.2 (redact tooling), 13.4 (<redacted> rendering), 17 (credentials), 19.10 (--json schemas), 19.11 (login, credentials), 19.13 (--all-versions, rendering)
- **Owns:** `crates/scion-cli/src/**`; `crates/scion-cli/tests/**`; `schemas/v1/**`
- **Deliverables:** Implement PLAN 2.18 for scion-cli with clap derive. Commands: - login: GetServerInfo for github_client_id and github_oauth_base_url, the device flow (scion_auth::device_flow, prompting with the user code and URL), Login, then the credentials file at 0600 with the GitHub token stored for silent re-exchange; on UNAUTHENTICATED from an expired token, re-exchange automatically. - whoami. - checkout <rev> [--sparse p...]; sparse add|remove [--force]; status; diff; commit -m [--path]; amend [-m]; goto <rev> [--force]; rebase --onto <rev> | --abort; resolve <path> [--force]; land [--ref main]. - log [path] [--first-parent] [--limit]; blame <path>; show <rev>; cat <rev>:<path>. - redact --path <p> --at <rev> [--all-versions] | --object <id> | --meta <rev>, with [--reason] [--purge-now]. --all-versions resolves every distinct blob via Log(path) plus ListTree/ReadPath entries per version. Revision display uses short ids (c1 + 12 hex) for humans and full ids in --json. Endpoint precedence: flag > SCION_ENDPOINT > .scion/config.toml > ~/.config/scion/config.toml. SCION_TOKEN overrides credentials. Redacted Meta renders as '<redacted>' in log and show. Every command supports --json, emitting Envelope{schema: 'scion.cli.<cmd>.v1', data}. Schemas are generated with schemars into schemas/v1/<cmd>.schema.json and committed. A test regenerates them and fails on any diff. Exit codes: 0 success, 1 error, 2 usage, 3 conflicts, 4 dirty-tree refusal (documented in the --help epilogue). Tests use assert_cmd plus run() against a TestCluster. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.10: the --json output of EVERY command validates against its published schema (jsonschema crate) in end-to-end runs against TestCluster, and the committed schemas match the generated ones. 19.11: device-flow login against FakeGitHub yields principal github:<login>; an invalid token gives an UNAUTHENTICATED exit; the credentials file is written 0600 and a group-readable file is refused. 19.13: redact --path --all-versions finds every distinct blob the path ever held (compared with scion-refmodel all_versions on a generated history); log and show render <redacted> after a Meta redaction; redact actions produce audit entries. Smoke: every command's human output on a scripted session.

##### P32: Conformance suites B1: land concurrency, linearizability, no-locks, whole-system differential against the reference model

- **Size:** L  |  **Depends on:** P09, P26, P30
- **SPEC sections:** 8.4, 8.5, 16.1 class 5, 19.0 (History Recorder), 19.4 (determinism across server processes), 19.5 (concurrency and linearizability)
- **Owns:** `crates/scion-conformance/tests/{land_concurrency.rs,linearizability.rs,differential.rs}`
- **Deliverables:** Cross-cutting suites over TestCluster (servers = 3 sharing one Storage, for multi-process semantics). land_concurrency.rs: N = 10 and 100 concurrent clients by default (N = 1000 in an #[ignore] full_ test) landing non-overlapping changes onto main via the Land RPC across the servers; overlapping text changes that merge cleanly; true conflicts. linearizability.rs: concurrent ResolveRef/UpdateRef/Land operations recorded with scion_testkit::lincheck::HistoryRecorder, under injected server kills (kill_server/restart_server mid-run; in-flight operations complete as Unknown) and network delays (a tower delay layer configured through ServerTestOpts.tweak or the fault registry's Delay at meta points). check_ref_registers must report zero violations over many seeds, with about 200 operations per history. differential.rs: a seeded random operation driver (create, rebase, merge, land, update_ref, redact) applied both to the real stack via RPC and to scion-refmodel; it compares trees (flattened), conflict path sets and kinds, ref-log order, error codes and landed sets, with at least 200 seeds by default (more under full_). Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.5: with N concurrent clients landing non-overlapping changes, all land, the final tree contains every delta exactly once, and each change id appears exactly once in landed. Overlapping clean merges: all land, and the final content equals scion-refmodel land_sequential applied in the landed order recorded in the ref log. True conflicts: exactly the conflicting lands fail MERGE_CONFLICT and the others land. Idempotency after a crash between CAS commit and response (FailAfter at meta.cas_ref.after): the retry returns the recorded result with no second version. No locks: the lease_calls delta is 0 and no scion::lock span appears during the concurrent land suite (LogCapture/tracing). Linearizability: zero violations with kills and delays. 19.4: Rebase gives the same id across different server processes and after restart. Whole-system differential: zero divergences. Every suite also runs on External when the env vars are set.

##### P33: Conformance suites B2: failure classes, complexity gates, write ordering at RPC level, redaction end to end, storage end to end, client end to end

- **Size:** L  |  **Depends on:** P22, P28, P29, P30
- **SPEC sections:** 4.5, 5.3, 13.3, 13.7, 16.1, 16.2, 19.3, 19.6 (eager values under faults), 19.10 (real stack), 19.13 (end to end), 19.16, 19.18
- **Owns:** `crates/scion-conformance/tests/{failures.rs,complexity.rs,write_ordering.rs,redaction_e2e.rs,storage_e2e.rs,client_e2e.rs}`
- **Deliverables:** Cross-cutting suites over TestCluster. failures.rs: one test per SPEC 16.1 class, each followed by invariants::check_16_2. 1. Metadata down (MemMetaStore fault Error on all meta points): writes give UNAVAILABLE; immutable reads continue from server caches (decoded LRU) and daemon cache; ref reads fail at the server while the daemon serves its TTL ref cache. 2. Object storage down (MemBlobStore.set_down): cache-miss reads fail; writes give UNAVAILABLE. 3. Crash mid-upload leaves harmless orphans. 4. Crash mid-CreateChangeset (Crash at each create.* point, then restart_server): no visible row; a retry yields the same id. 5. Crash after the CAS commit, before the response: Land retry returns the recorded result. 6. Daemon crash (kill/restart InProcessDaemon): temp files deleted, cache valid. 7. Corrupt object at each tier: discarded and refetched; the server reports INTEGRITY and scion_integrity_failures_total. 8. Client crash mid-operation (wc fault points): recovered by the next command. 9. Invalid policy reload: the last good policy is retained and the error surfaced. 10. GitHub down: existing tokens work and new logins fail. complexity.rs: every 19.18 gate end to end via RPC on shared_subtree_repo at 10^3 and 10^5 files (10^7 in an #[ignore] full_ test) with equal depth, using Counters deltas only: - CreateChangeset 1-file: reads at most depth+c trees, writes at most depth+1. - Rebase over an unrelated tip: 0 blobs, O(|delta| x depth) trees. - Land 1-file: constant meta_txns per attempt. - Sparse checkout of a fixed directory: same object count at every size. - Clean status: 0 upstream reads (ScionClient.calls) and only racy or stat-changed files hashed. - Eager derivation: at most depth+1 history nodes. - Object authorization: object_paths rows read only for the requested object. write_ordering.rs: crash injection at every Checkpoint and every meta/blob decorator point during upload, CreateChangeset and Land via RPC. No visible changeset violates 4.5 (fsck with required_eager); eager values exist for every visible changeset (19.6). redaction_e2e.rs: the full runbook through the real stack and the daemon: redact, the server returns redacted by effective_by, the daemon by one poll interval plus one request with its cached copy deleted, daemon fail-closed, purge, Meta redaction rendered in Log, re-introduction blocked, carry-forward allowed, and working-copy placeholder replacement. storage_e2e.rs: the 19.3 items at RPC level (a row never exists without bytes, inline threshold +-1 via UploadObjects/CreateChangeset, the stale-read retry, orphans harmless). client_e2e.rs: re-run the scion_wc::testing and testing_write scenario functions (P18/P28) against the real stack, directly and via the daemon. All suites run on External when the env vars are set. Definition of done: cargo fmt --check, cargo build --workspace --all-targets, cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings and scripts/check-layers.sh all pass (scripts/check.sh), only owned files are touched, and every frozen signature in docs/PLAN.md section 2 is unchanged. Additive pub items are allowed only inside owned files; any other contract change goes into the final report as an amendment request.
- **SPEC 19.x tests:** 19.16: each of the 10 failure classes with the 16.2 invariants checked afterwards. 19.18: all seven gates at 10^3 and 10^5 (10^7 under full_). 19.3: write ordering across every crash point and storage items at RPC level. 19.6: eager values exist under faults between derive and insert. 19.13: end-to-end propagation bounds (server effective_by; daemon one poll interval plus one request; fail-closed), runbook flow, working-copy replacement. 19.10: client scenarios pass on the real stack directly and via the daemon.


## 5. Conventions

### 5.1 Code

- **Toolchain:** Rust 1.98 (pinned in `rust-toolchain.toml`), edition 2024, resolver 3. Formatting is rustfmt with default settings; `cargo fmt --all -- --check` must pass.
- **Lints:** `cargo clippy --workspace --all-targets -- -D warnings`. The workspace lints apply to every crate:
  - `unsafe_code = "deny"`.
  - clippy `unwrap_used`, `expect_used` and `panic` are denied.
  - `clippy.toml` sets `allow-unwrap-in-tests = true` and `allow-expect-in-tests = true`.
  - **No `unwrap`/`expect`/`panic!` in library code.** `todo!()` exists only in unimplemented stubs. A narrowly scoped `#[allow]` with a `// SAFETY-ish:` justification comment is allowed only for truly unreachable states.
- **Errors:**
  - Library crates use `thiserror`. `scion_core::ScionError` is the cross-crate error, and every public fallible API returns `scion_core::Result`. Local error enums (`PolicyError`, `ConfigError`) convert into it.
  - Binaries (`main.rs`) and tests may use `anyhow`.
  - Never stringly-type an error code: use `ErrorCode` plus the `subcode` constants.
- **Async:** `tokio` multi-thread runtime; `#[tokio::test(flavor = "multi_thread")]` for concurrency tests. Async traits use `async-trait` (object-safe). Never block the runtime: filesystem-heavy client and cache work uses `spawn_blocking` where needed.
- **Logging:**
  - `tracing` only; no `println!` outside CLI output.
  - RPC events go through `scion_obs::RpcLogEvent`.
  - Never log secrets, tokens, file contents or raw repository paths; use `path_hash` (§6.9).
  - Lease/lock acquisitions emit a span with target `scion::lock`.
- **Determinism:**
  - Anything that affects ids, ordering or output uses `BTreeMap`/sorted vectors, never `HashMap` iteration order.
  - Time comes only from `Clock`.
  - Randomness is seedable: `rand_chacha`, with `ChangeId::random` the only OS-random use in production.
  - No wall-clock time in rebased changesets.
- **Payloads and paths:** `bytes::Bytes` for payloads. Paths are `RepoPath` (bytes), never `String`.
- **Docs:** every `pub` item has rustdoc citing its SPEC section (e.g. `/// SPEC 8.4 Land.`).
- **Tests:**
  - No network except loopback.
  - Temp dirs via `tempfile`.
  - Heavy counts via `scion_core::test_support::proptest_cases`, with spec-sized `#[ignore] full_*` variants.
  - Gated external tests are named `external_*` and print `SKIP:` when their env vars are unset.
  - Tests that need the `git` CLI print `SKIP:` when it is absent.

### 5.2 Ownership and change control

- Only P00 edits root files, any `Cargo.toml`, `Cargo.lock`, any `src/lib.rs`, the proto, and the frozen trait files.
- From wave 1 on, a package edits only its `owns` entries. It may add private files under its own module directories and test files matching its prefixes.
- No package adds a crate. If an amendment ever adds one, it adds the crate under `crates/` and appends exactly one line to the workspace `members` list. The orchestrator applies that amendment.
- **Amendments:** a package states the request (file, item, old signature, new signature, reason) in its final report. The orchestrator batches requests, applies them between waves in one serialized patch, regenerates `Cargo.lock` if needed, and logs them in `docs/contracts/amendments.md`. Additive `pub` items inside a package's own files need no amendment.
- **Workflow:** each package runs in its own git worktree/branch created from the wave base, which is the merged result of all previous waves. The orchestrator merges a wave's branches only when each passes `scripts/check.sh`, and re-runs `scripts/check.sh` on the merged result before starting the next wave.

### 5.3 Workspace dependencies (pinned by P00 in `[workspace.dependencies]`; versions indicative minimums, exact pins resolved at scaffold time)

| Crate | Indicative version | Used by |
|---|---|---|
| tokio (full), tokio-stream, futures | 1, 0.1, 0.3 | all async crates |
| tonic (transport, tls-ring, codegen), prost, tonic-prost, tonic-prost-build, prost-types | 0.14 / 0.14 | proto, server, daemon, testkit, wc, cli, import |
| tower (limit, load-shed, util), tower-layer, tower-service, http, http-body, http-body-util, hyper, hyper-util | 0.5 / 1 / 1 / 0.1 | proto, server, daemon |
| rustls, tokio-rustls, rustls-pemfile, rcgen | 0.23 / 0.26 / 2 / 0.14 | proto, server, daemon, testkit, store-crdb |
| blake3, zstd | 1 / 0.13 | core, object, store, cache, refmodel (blake3 only) |
| bytes, async-trait, thiserror, anyhow | 1 / 0.1 / 2 / 1 | all |
| serde (derive), serde_json, toml, serde_path_to_error, humantime, humantime-serde | 1 / 1 / 0.9 / 0.1 / 2 / 1 | config, policy, wc, cli, obs |
| tokio-postgres, deadpool-postgres, tokio-postgres-rustls | 0.7 / 0.14 / 0.13 | store-crdb |
| object_store (aws) | 0.12 | store-s3 |
| parking_lot, dashmap, im, lru, arc-swap | 0.12 / 6 / 15 / 0.16 / 1 | store, cache, server, policy, derived |
| notify | 8 | policy |
| governor | 0.10 | server |
| pasetors (v4 public), ed25519-dalek (pkcs8, pem), sha2, subtle, hex | 0.7 / 2 / 0.10 / 2 / 0.4 | auth, server, core (hex) |
| reqwest (rustls-tls, json) | 0.12 | auth |
| axum | 0.8 | obs, testkit |
| prometheus | 0.14 | obs |
| tracing, tracing-subscriber (json, env-filter) | 0.1 / 0.3 | all |
| rustix (fs), ignore, fs4, tempfile | 1 / 0.4 / 0.13 / 3 | wc, cache |
| gix (max-performance-safe) | 0.7x | import, testkit |
| clap (derive), schemars, jsonschema, assert_cmd | 4 / 1 / 0.3x / 2 | cli |
| rand, rand_chacha | 0.9 / 0.9 | core, testkit, history |
| proptest, wiremock, similar (dev only, scion-merge) | 1 / 0.6 / 2 | dev-deps |
| uuid (v4) | 1 | server, daemon (request ids) |

## 6. Implementation-defined behaviour (resolved; P00 copies this into `docs/implementation-defined.md`)

### 6.1 Tokens and keys

- Encoding is PASETO v4.public (Ed25519, via pasetors).
- Claims: `sub` (principal id string), `gid` (GitHub numeric id, optional), and `iat`/`exp` as RFC 3339 timestamps.
- The footer is JSON `{"kid":"<16 hex>"}`, where kid is the first 16 hex characters of BLAKE3(public key).
- Signing keys are PKCS#8 PEM Ed25519 files. The first configured key signs; all configured keys verify.
- The default TTL is 12h.

### 6.2 Epochs

- `policy_epoch` is the first 8 bytes, big-endian, of BLAKE3 over the canonical normalized policy: sorted groups and members, prefixes and refs normalized, principals lowercased for `github:`.
- `redaction_epoch` is `counters.redaction_epoch`.
- Both trailer values are decimal u64 strings. `scion-tier` is `server` or `daemon`.

### 6.3 line-diff3-v1 (normative contract summary)

- Lines are split on `\n` with terminators kept, and compared bytewise.
- Myers runs forward, greedy, O(ND). For d = 0, 1, ...: for k = -d, -d+2, ..., d:
  - If `k == -d || (k != d && V[k-1] < V[k+1])`, then `x = V[k+1]` (insertion).
  - Otherwise `x = V[k-1] + 1` (deletion).
  - Set `y = x - k`, follow the snake while lines are equal, and set `V[k] = x`.
- Backtracking uses the same rule. There are no heuristics and no cost cap.
- Within a changed run, deletions precede insertions.
- diff3: build hunks base->ours and base->theirs. Form regions as the transitive closure of hunks whose base ranges overlap or touch (`end == start`; an empty insertion range at, or inside, another range touches it).
  - A region with hunks from one side only is applied.
  - A two-sided region whose resulting texts are identical is applied once.
  - Any other two-sided region is a conflict, carrying base/ours/theirs line ranges.
- Output is byte-exact. The contract doc's worked examples and `testdata/golden/diff` are authoritative. If an implementation and the doc disagree, the doc wins and the implementation is fixed.

### 6.4 Importer

- `CreateChangesetRequest.import_mode` requires `admin.refs` and write on `/`. It permits parentless changesets beyond the root (SPEC 14), and it enables eager `git_mapping` derivation from Meta extension tag 2.
- `git_mapping` rows are keyed `g:<40hex>` -> changeset and `c:<changeset id bytes>` -> 40hex. A changeset created without `import_mode` never creates mappings, even when its Meta carries tag 2.
- Skipped gitlinks are recorded in Meta extension tag 4 (even, so ignorable): `list<(path bytes, 20-byte sha)>`.
- `change_id = BLAKE3("scion-import-v1" || 20-byte id)[0..16]`.
- The importer authenticates as a service principal (SCION_SERVICE_NAME/SCION_SERVICE_SECRET) or with SCION_TOKEN.

### 6.5 Repository bootstrap

On startup the server idempotently ensures:

- The `repos` row.
- The root changeset: empty root tree; Meta author `scion <scion@localhost>`, time 0/0, creator `service:scion`, message `root`, no committer; change_id `BLAKE3("scion-root-v1")[0..16]`.
- Ref `main` at version 1 pointing to the root changeset, created via `cas_ref(Absent)`.

### 6.6 Identifier text and revision resolution

- Derived kind 0x10 prints as `x1<64hex>`. Other derived kinds (none in v1) print as `x1<2-hex kind><64hex>`.
- Changeset prefixes are accepted as `c1<8..63 hex>` or as bare hex of at least 8 digits.
- For a bare token, an existing ref with that exact name wins. Otherwise it is tried as a hex prefix.
- An ambiguous prefix returns INVALID_ARGUMENT/ambiguous_prefix with a Candidates payload.
- A bare `~` or `^` means 1. `^0` is invalid.

### 6.7 Error choices not fixed by the SPEC

| Situation | Result |
|---|---|
| Unredact after purge | INVALID_ARGUMENT / not_quarantined |
| Redacting a Tree or Changeset | INVALID_ARGUMENT / not_redactable |
| Redacting an unknown id | NOT_FOUND |
| Stack containing a merge | INVALID_ARGUMENT / merge_in_stack |
| Rebase of a multi-parent changeset | INVALID_ARGUMENT / multi_parent |
| `land.max_attempts` exhausted | UNAVAILABLE / land_contention (retryable) |
| Creating an existing ref | INVALID_ARGUMENT / ref_exists |
| Missing required proto field | INVALID_ARGUMENT / missing_field |
| Limit exceeded | TOO_LARGE with a Limit payload naming the config key |
| Injected faults | UNAVAILABLE / injected or injected_crash |

### 6.8 Audit log

- An extra metadata table: `audit_log(repo_id, seq, at, actor, action, target, reason, detail)`. It is append-only and identity-free (allowed by SPEC 5.2).
- It is written in the same transaction as the audited action: ref create/delete, forced updates, redact/unredact/purge.

### 6.9 Log access policy (SPEC 15.1)

- Server and daemon logs are operator-only artifacts, never served through Scion APIs.
- Raw repository paths are never logged at any level. Where a path is useful, log `path_hash` (16 hex characters of BLAKE3) instead. Restricted path names therefore cannot leak to log readers who lack access.
- Tokens, secrets (the `Secret` type) and file contents are never logged.
- P30's full-suite log scan enforces this.

### 6.10 Integrity alerting (SPEC 16.1 class 7)

When a tier detects that its own stored copy is corrupt, it:

- increments `scion_integrity_failures_total{tier}`;
- emits an ERROR event with `event="integrity_alert"` and the object id;
- returns INTEGRITY (retryable).

### 6.11 Additional configuration keys

These are allowed because the config schema is ours. Unknown keys are still rejected.

- `[rate_limit] per_principal_rps = 500`, `burst = 1000`, `max_inflight = 4096`.
- `[objects] region = "us-east-1"`, `path_style = true`.
- `[auth] github_oauth_base_url = "https://github.com"`.
- Daemon: `upstream_queue = 4096`, `upstream_ca` (optional CA path).

### 6.12 Authorization details not fixed by the SPEC

- **Rebase / IsAncestor / MergeBase / GetDerived / Resolve:** require repository read (read on `/`).
- **ListRedactions:** requires any valid token. It reveals ids only, never content, and daemons poll it.
- **CheckAccess:** requires any valid caller token. The answer concerns `subject_token`.
- **GetObjects for trees:** authorized through `object_paths` rows of the tree at its directory path. The root tree sits at the empty path, governed by `/`.

### 6.13 Daemon forwarding and tiers

- Non-object services are forwarded byte-for-byte (RawCodec), headers and trailers included. Forwarded responses therefore carry the server's `scion-tier: server`, which satisfies "forwarded without modification" (7.8). Daemon-served ObjectService responses carry `scion-tier: daemon`.
- `HasObjects` is answered by forwarding, because its semantics depend on server-side upload records.
- A principal is confirmed as a root reader by its first positive CheckAccess on a root-class object. That is sound because root class means the governing read rule is `/`. The confirmation is cached for `access_cache_ttl_ms` and dropped on a policy epoch change.

### 6.14 Merge and land edge semantics

- `Merge(a, b)` with zero common ancestors (unrelated imported roots) uses the empty tree as base. More than one best base gives AMBIGUOUS_MERGE_BASE.
- `Land(..., allow_empty = true)`: a member whose rebase is EMPTY_CHANGE is recorded in `landed` with `result` = the current onto changeset, without creating a new changeset, so retries stay idempotent.

### 6.15 Client formats

| Item | Format |
|---|---|
| `.scion/state` | Canonical TOML: keys sorted, deterministic serialization |
| `.scion/index` | Versioned binary stat cache |
| `.scion/config.toml` | `endpoint`, `tracking_ref` |
| Credentials | TOML `{token, github_token?}`, mode 0600; reading refuses group/world-readable files |
| Placeholder | Exactly `Scion: contents redacted (object <id text>).\n` |
| CLI exit codes | 0 ok, 1 error, 2 usage, 3 conflicts, 4 dirty refusal |
| `--json` envelope | `{"schema":"scion.cli.<cmd>.v1","data":...}`; schemas in `schemas/v1/` |

### 6.16 Backups (SPEC 13.8)

`docs/operations/backup-retention.md` is owned by P22. It covers retention of at most 30 days for CockroachDB backups and object-store versions, the redaction runbook, and verification steps.

### 6.17 Case collisions

- Checkout computes ASCII case-fold collisions over the entries to be written whenever the filesystem probes as case-insensitive, or `force_case_insensitive` is set. Collisions are reported, and the colliding entries are not written.
- Detection is deterministic on every host. On case-insensitive hosts (macOS APFS) a real-filesystem test also runs.

## 7. Risks and open decisions (all resolved)

### 7.1 Risks and mitigations

1. **P00 is a single XL bottleneck, and wrong contracts spread everywhere.**
   - §2 gives exact signatures to transcribe, and P00 self-reviews.
   - Gate G0 (a fresh-agent review) follows before wave 1.
   - Additive changes need no amendment, and amendments are applied between waves.
   - Every assembly constructor is real in P00, so harnesses never panic on wiring.
2. **Hidden runtime dependencies on same-wave `todo!()`s.** All judge-identified cases are fixed:
   - Graph queries read rows directly instead of calling `create_changeset`.
   - The derived framework does not need `History`.
   - The server core takes a `registry` knob, so it does not depend on P21.
   - The purge scheduler is spawned from P25's hook.
   - Land is tested with a fake `Rebaser`.
   - Client fetch, checkout and the daemon are tested against `FakeUpstream`.
   - Working-copy recovery dispatch for Commit/Rebase lands with P28.
   - The rule in §4.1 forbids tests from exercising non-dependency code.
3. **Merge tie-breaking divergence between production and the oracle.**
   - The normative contract doc with worked examples is written in P00, before either implementation.
   - The golden vectors are frozen.
   - P17 cross-checks in wave 2, so divergence surfaces early.
   - The doc wins ties.
4. **The in-memory store may hide CockroachDB anomalies (40001, follower staleness, unique violations inside multi-statement transactions).**
   - Coarse typed one-transaction methods leave little room for drift.
   - Mem models stale snapshots and forced transaction failures.
   - The same conformance suite runs against CRDB when gated.
   - The External variants of all conformance suites run in a nightly CI job (`docker/compose.test.yml`).
   - Residual risk: CRDB is not run by default `cargo test`. Accepted because the task requires hermetic tests.
5. **Test runtime (10^6 cases, 1000-way lands, 10^7 files).** Counts are scaled by default. Spec-sized runs are `#[ignore] full_*` tests run by `scripts/check.sh --full` (nightly). The 10^7 gate uses identical-subtree dedup and counter-only assertions.
6. **Success trailers in tonic.** Unary metadata lands in headers, so a body-level `EpochTrailerLayer` appends to the HTTP/2 trailers frame (or to trailers-only headers). It is implemented and tested in wave 1 by P08 over TLS and Unix sockets before any server code depends on it.
7. **Daemon verbatim forwarding.** The RawCodec byte proxy is chosen by gRPC path. P08 tests it, and P16 and P30 test it again. Fallback (only via amendment): typed re-serialization, documented as a deviation.
8. **Real OSS git corpora need network access and licence review.** P09 names concrete MIT/Apache candidates (rust-lang/cfg-if, dtolnay/itoa), checks size and licence, and vendors bundles plus synthetic edge bundles. Tests then never need the network or the git CLI. If the network is unavailable P09 stops and reports; it never fakes the requirement.
9. **Linearizability checker blow-up.** Histories are partitioned per ref, capped at about 200 operations, memoized WGL search is used, and many seeds are run.
10. **Platform differences (macOS vs Linux).** rustix provides `openat`/`O_NOFOLLOW`. Case-collision detection is deterministic (§6.17). Policy reload uses a polling fallback, and tests call `reload_now`.
11. **Fault points in production code.** Unarmed checks are a single relaxed atomic load, registries are inert unless armed, and storage faults live in decorators.
12. **Parallel merges conflicting on `Cargo.lock` / `lib.rs`.** Both are frozen in P00. Only the orchestrator touches them, via amendments.

### 7.2 Decisions resolved

| Question | Decision | Rationale |
|---|---|---|
| Base architecture | Proposal A (layered crates, stub freeze, layer check), with B/C grafts | Consensus winner of 2 of 3 judges; strongest ownership protocol |
| Metadata client | tokio-postgres + deadpool-postgres (not sqlx) | Explicit SERIALIZABLE/40001 control and follower reads; no compile-time DB needed, so builds stay hermetic |
| Object store client | `object_store` (aws, path-style) behind `BlobStore` | Light, MinIO-friendly; swappable locally |
| Git reader | gix (not git2) | Pure Rust, no C dependency |
| Metrics | `prometheus` crate | Direct value access for "metric moves" tests |
| Separate domain API layer (B's scion-api) | Rejected | Avoids a double conversion layer; handlers call engines directly; in-process tests use `InProcessServer` |
| Per-engine error enums (C) | Rejected | One `ScionError` with `ErrorCode` + subcodes avoids mapping churn |
| RPC-level ModelServer oracle (C) | Replaced by `FakeUpstream` (read paths) + `InProcessServer` | A second full service implementation costs too much and diverges; the refmodel differential (P32) still compares whole-system behaviour |
| git_mapping write path | Eager derivation from Meta tag 2 under `import_mode` | The importer stays RPC-only with no new RPC |
| Generic derived query path (19.6) | Additive `GetDerived` RPC | Concrete, testable RPC-level path |
| GitHub client id discovery (12.2) | `ServerInfo.github_client_id` + `github_oauth_base_url` | Required by the device flow; additive fields |
| Load shedding (10.5) | tower concurrency limit + load_shed, plus governor per principal | Both map to RESOURCE_EXHAUSTED |
| Lock-free proof (19.5) | `Counters.lease_calls` plus a `scion::lock` span convention frozen in P00 | Mechanically assertable |
| Repository bootstrap | `History::bootstrap` at server start (§6.5) | Closes the gap the judges found |
| Case-collision portability | Deterministic case-fold detector + real APFS test | Passes on case-sensitive CI |
| Server immutable caches during a metadata outage (16.1-1) | ObjectRepo decoded LRU + daemon cache; tested in P33 | Closes the gap the judges found |
| Optional git merge-file differential (19.4 RECOMMENDED) | Included in P02, skipped without git | Cheap; disagreements documented |
| OpenTelemetry (15.3 RECOMMENDED) | Deferred | Not required for 20.1 |
| Package count / waves | 34 packages, 7 waves; wave 1 has 11 parallel packages | Critical path shortened from 9 waves to 7 by the Rebaser seam and FakeUpstream |

## Appendix A. SPEC 19.x coverage map

| SPEC | Packages (where the tests live) |
|---|---|
| 19.0 Required infrastructure | P07 (Reference Model), P09 (Corpus, History Recorder/lincheck, real git bundles), P03 (Storage Access Counters, fault decorators, fsck), P00 (FaultRegistry, checkpoint names), P08/P06 (FakeUpstream, TestTls, FakeGitHub), P23/P16/P30 (in-process server, daemon, TestCluster) |
| 19.1 Encoding and identifiers | P00 (id text, kinds), P01 (codec, golden, round trip, rejections, even tags), P03 (blob id vs compression), P17 (refmodel agreement), P27 (ambiguous prefix with candidates) |
| 19.2 Tree construction and validity | P01 (path rules), P12 (property vs refmodel, every subcode, transitions, idempotency, expected_root_tree), P26 (creator server-set, RPC-level) |
| 19.3 Storage layer | P03 (ordering, inline threshold, stale retry, orphans), P12 (crash between 5.3 steps), P15 (real backends), P33 (RPC level) |
| 19.4 Rebase and merge | P02 (content properties, binary boundary, CRLF, git merge-file), P17 (10^6 differential), P19 (every table row, structural, determinism, fast path, EMPTY_CHANGE, zero blob reads, payloads, no markers, Merge vs refmodel, criss-cross), P26/P32 (determinism across restart and processes) |
| 19.5 Refs and land | P03 (store CAS), P20 (ABA, ff, idempotency, ALREADY_LANDED, partial stack, merge rejected, re-auth per attempt, max_attempts, no locks), P26 (1000-way UpdateRef via RPC), P32 (N=10/100/1000, clean overlaps vs refmodel, true conflicts, linearizability, no-lock traces) |
| 19.6 Derived data | P13 (commit_graph vs refmodel), P14 (determinism, 10^5 lazy, version bump, test-only type), P21 (history nodes, reuse, eager under faults), P24 (blame, Log(path), version via RPC), P27 (GetDerived generic path), P29 (blame vs git blame), P33 (eager under faults end to end) |
| 19.7 RPC contracts | P00, P08 (mapping, forward compatibility), P23/P24/P25/P26/P27 (per RPC), P30 (descriptor-driven complete matrix) |
| 19.8 Tier conformance | P16 (vs FakeUpstream), P30 (real server, network policy) |
| 19.9 Daemon cache | P10 (unit), P16 (daemon level) |
| 19.10 Fetch and working copy | P11 (status, ignore, path safety, case collisions), P18 (sparse set, resume, materialization, malicious trees, goto, zero calls, sparse add/remove, inaccessible, crash in goto), P28 (commit, amend, conflict flow, crash in commit/rebase), P31 (--json schemas), P30/P33 (real stack via daemon) |
| 19.11 Authentication | P06 (tokens, rotation, ct primitive), P11 (credentials permissions), P23 (server logins, token rejection), P16 (daemon token rejection), P30 (end to end), P31 (device-flow CLI login) |
| 19.12 Access control | P05 (semantics, reload), P23 (reload epoch trailer), P25 (retroactivity, existence probe, unreferenced, admin perms), P26 (write checks), P27/P24 (history filtering), P16 (daemon isolation), P30 (full matrix) |
| 19.13 Redaction | P22 (lifecycle, purge, audit), P12 (Put redacted blocked, carry-forward), P19 (redacted conflict), P25 (server timing, re-upload blocked, audit), P27 (ReadPath REDACTED), P24 (all-versions, Meta rendering), P16 (daemon timing, fail-closed), P18 (placeholders, replacement), P31 (--all-versions, <redacted>), P33 (end to end) |
| 19.14 Git import | P29 (+ P09 corpora) |
| 19.15 Configuration | P04 |
| 19.16 Failure injection | P33 (all 10 classes), with P23 (GitHub down), P16 (daemon crash), P18/P28 (client crash) |
| 19.17 Observability | P04 (unit), P23 (RPC events), P30 (full log scan, metrics move, health) |
| 19.18 Complexity gates | P12 (create), P19 (rebase), P20 (land txns), P21 (history nodes), P25 (object authz), P18 (sparse checkout, status), P33 (all gates end to end at 10^3/10^5/10^7) |

## Appendix B. SPEC 20.1 checklist map

| 20.1 item | Packages |
|---|---|
| Canonical encoding, identifiers, golden vectors | P00, P01, P17 |
| Object/tree/Meta/changeset validation incl. all 8.1 subcodes | P01, P12, P03 (decoded-form validation on upload) |
| CockroachDB schema and 5.3 write ordering | P00 (DDL contract), P15, P03, P12 |
| Object storage layout with inline threshold | P03, P15 |
| Server-side tree construction in O(\|delta\| x depth) | P12 |
| Deterministic rebase and three-way merge (line-diff3-v1) | P02, P19 |
| Lock-free Land with idempotent retries and landed index | P20, P26, P32 |
| Version-checked UpdateRef with fast-forward enforcement and ref log | P20, P26 |
| Derived framework + commit_graph, history, blame, path_index, git_mapping | P14, P12 (commit_graph columns, path_index rows), P13, P21, P24 |
| Query RPCs ReadPath, ListTree, Log, Blame, Diff, IsAncestor, MergeBase, Resolve | P27, P24 |
| scion-daemon (verified cache, coalescing, eviction, ref TTL, epochs, tier conformance) | P10, P16, P30 |
| Client: checkout, sparse, goto, status, commit, amend, rebase + conflicts, land, log, blame, show, cat, --json | P11, P18, P28, P31 |
| GitHub device-flow login, service principals, signed tokens verified by server and daemon | P06, P23, P16, P31 |
| acl.toml with reload; retroactive object authz; history filtering; existence-probe protection | P05, P23, P25, P27, P24 |
| Redaction lifecycle, propagation bounds, fail-closed daemon, placeholders, re-introduction blocking | P22, P23 (RedactionSet), P25, P16, P18, P12 |
| scion-import with fidelity | P29 |
| Structured logs, 15.2 metrics, health endpoints | P04, P23, P16, P30 |
| All 19.1-19.18 tests passing | Appendix A; final gate: `scripts/check.sh --full` on the merged wave-6 result (plus `scripts/test-external.sh` with Docker) |
