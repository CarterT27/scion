# Implementation-defined behaviour

This document records every behaviour the SPEC leaves to the implementation, as resolved in
`docs/PLAN.md` section 6. It is a verbatim transcription of that section (headings shifted one
level); `docs/PLAN.md` remains the source. Changes go through the amendment protocol
(`docs/contracts/amendments.md`).

Related normative contracts:

- `docs/contracts/line-diff3-v1.md` (the merge engine, PLAN 6.3).
- `docs/contracts/storage-semantics.md` (metadata transactions, object layout, write ordering).


## 6.1 Tokens and keys

- Encoding is PASETO v4.public (Ed25519, via pasetors).
- Claims: `sub` (principal id string), `gid` (GitHub numeric id, optional), and `iat`/`exp` as RFC 3339 timestamps.
- The footer is JSON `{"kid":"<16 hex>"}`, where kid is the first 16 hex characters of BLAKE3(public key).
- Signing keys are PKCS#8 PEM Ed25519 files. The first configured key signs; all configured keys verify.
- The default TTL is 12h.

## 6.2 Epochs

- `policy_epoch` is the first 8 bytes, big-endian, of BLAKE3 over the canonical normalized policy: sorted groups and members, prefixes and refs normalized, principals lowercased for `github:`.
- `redaction_epoch` is `counters.redaction_epoch`.
- Both trailer values are decimal u64 strings. `scion-tier` is `server` or `daemon`.

## 6.3 line-diff3-v1 (normative contract summary)

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

## 6.4 Importer

- `CreateChangesetRequest.import_mode` requires `admin.refs` and write on `/`. It permits parentless changesets beyond the root (SPEC 14), and it enables eager `git_mapping` derivation from Meta extension tag 2.
- `git_mapping` rows are keyed `g:<40hex>` -> changeset and `c:<changeset id bytes>` -> 40hex. A changeset created without `import_mode` never creates mappings, even when its Meta carries tag 2.
- Skipped gitlinks are recorded in Meta extension tag 4 (even, so ignorable): `list<(path bytes, 20-byte sha)>`.
- `change_id = BLAKE3("scion-import-v1" || 20-byte id)[0..16]`.
- The importer authenticates as a service principal (SCION_SERVICE_NAME/SCION_SERVICE_SECRET) or with SCION_TOKEN.

## 6.5 Repository bootstrap

On startup the server idempotently ensures:

- The `repos` row.
- The root changeset: empty root tree; Meta author `scion <scion@localhost>`, time 0/0, creator `service:scion`, message `root`, no committer; change_id `BLAKE3("scion-root-v1")[0..16]`.
- Ref `main` at version 1 pointing to the root changeset, created via `cas_ref(Absent)`.

## 6.6 Identifier text and revision resolution

- Derived kind 0x10 prints as `x1<64hex>`. Other derived kinds (none in v1) print as `x1<2-hex kind><64hex>`.
- Changeset prefixes are accepted as `c1<8..63 hex>` or as bare hex of at least 8 digits.
- For a bare token, an existing ref with that exact name wins. Otherwise it is tried as a hex prefix.
- An ambiguous prefix returns INVALID_ARGUMENT/ambiguous_prefix with a Candidates payload.
- A bare `~` or `^` means 1. `^0` is invalid.

## 6.7 Error choices not fixed by the SPEC

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

## 6.8 Audit log

- An extra metadata table: `audit_log(repo_id, seq, at, actor, action, target, reason, detail)`. It is append-only and identity-free (allowed by SPEC 5.2).
- It is written in the same transaction as the audited action: ref create/delete, forced updates, redact/unredact/purge.

## 6.9 Log access policy (SPEC 15.1)

- Server and daemon logs are operator-only artifacts, never served through Scion APIs.
- Raw repository paths are never logged at any level. Where a path is useful, log `path_hash` (16 hex characters of BLAKE3) instead. Restricted path names therefore cannot leak to log readers who lack access.
- Tokens, secrets (the `Secret` type) and file contents are never logged.
- P30's full-suite log scan enforces this.

## 6.10 Integrity alerting (SPEC 16.1 class 7)

When a tier detects that its own stored copy is corrupt, it:

- increments `scion_integrity_failures_total{tier}`;
- emits an ERROR event with `event="integrity_alert"` and the object id;
- returns INTEGRITY (retryable).

## 6.11 Additional configuration keys

These are allowed because the config schema is ours. Unknown keys are still rejected.

- `[rate_limit] per_principal_rps = 500`, `burst = 1000`, `max_inflight = 4096`.
- `[objects] region = "us-east-1"`, `path_style = true`.
- `[auth] github_oauth_base_url = "https://github.com"`.
- Daemon: `upstream_queue = 4096`, `upstream_ca` (optional CA path).

## 6.12 Authorization details not fixed by the SPEC

- **Rebase / IsAncestor / MergeBase / GetDerived / Resolve:** require repository read (read on `/`).
- **ListRedactions:** requires any valid token. It reveals ids only, never content, and daemons poll it.
- **CheckAccess:** requires any valid caller token. The answer concerns `subject_token`.
- **GetObjects for trees:** authorized through `object_paths` rows of the tree at its directory path. The root tree sits at the empty path, governed by `/`.

## 6.13 Daemon forwarding and tiers

- Non-object services are forwarded byte-for-byte (RawCodec), headers and trailers included. Forwarded responses therefore carry the server's `scion-tier: server`, which satisfies "forwarded without modification" (7.8). Daemon-served ObjectService responses carry `scion-tier: daemon`.
- `HasObjects` is answered by forwarding, because its semantics depend on server-side upload records.
- A principal is confirmed as a root reader by its first positive CheckAccess on a root-class object. That is sound because root class means the governing read rule is `/`. The confirmation is cached for `access_cache_ttl_ms` and dropped on a policy epoch change.

## 6.14 Merge and land edge semantics

- `Merge(a, b)` with zero common ancestors (unrelated imported roots) uses the empty tree as base. More than one best base gives AMBIGUOUS_MERGE_BASE.
- `Land(..., allow_empty = true)`: a member whose rebase is EMPTY_CHANGE is recorded in `landed` with `result` = the current onto changeset, without creating a new changeset, so retries stay idempotent.

## 6.15 Client formats

| Item | Format |
|---|---|
| `.scion/state` | Canonical TOML: keys sorted, deterministic serialization |
| `.scion/index` | Versioned binary stat cache |
| `.scion/config.toml` | `endpoint`, `tracking_ref` |
| Credentials | TOML `{token, github_token?}`, mode 0600; reading refuses group/world-readable files |
| Placeholder | Exactly `Scion: contents redacted (object <id text>).\n` |
| CLI exit codes | 0 ok, 1 error, 2 usage, 3 conflicts, 4 dirty refusal |
| `--json` envelope | `{"schema":"scion.cli.<cmd>.v1","data":...}`; schemas in `schemas/v1/` |

## 6.16 Backups (SPEC 13.8)

`docs/operations/backup-retention.md` is owned by P22. It covers retention of at most 30 days for CockroachDB backups and object-store versions, the redaction runbook, and verification steps.

## 6.17 Case collisions

- Checkout computes ASCII case-fold collisions over the entries to be written whenever the filesystem probes as case-insensitive, or `force_case_insensitive` is set. Collisions are reported, and the colliding entries are not written.
- Detection is deterministic on every host. On case-insensitive hosts (macOS APFS) a real-filesystem test also runs.


## Appendix: further choices fixed by the P00 contract documents

These are not in PLAN section 6; they are fixed by the normative contracts named and listed here
so that this file stays the single index of implementation-defined behaviour.

| Topic | Choice | Where |
|---|---|---|
| Repository id | `i64::from_be_bytes(BLAKE3(repo_name)[0..8]) & i64::MAX` | storage-semantics 1.5 |
| Re-creating a deleted ref | The version continues after the last `ref_log` version of that name (1 for a new name) | storage-semantics 4.3 |
| Unredaction propagation | Unredact deletes the `redactions` rows and bumps the epoch; consumers re-list from epoch 0 when epochs are unaccounted for | storage-semantics 4.5 |
| Redacting an already purged object | Leaves it purged; a quarantined object has its reason/purge_after/epoch refreshed | storage-semantics 4.5 |
| Changeset storage | Changesets live only in `changesets.encoded`, never as object-storage objects | storage-semantics 7 |
| Object keys of derived kinds | `x` for kind 0x10, `x<2-hex kind>` otherwise | storage-semantics 7 |
| Myers diagonals | Not clamped to the edit grid; tie (`V[k-1] == V[k+1]`) selects the deletion branch | line-diff3-v1 2 |
| Conflict marker lines | A `\n` is inserted before a marker when the preceding content lacks one | line-diff3-v1 8 |
| `ErrorPayload::Candidates` | `ScionError::subcode()` reports `ambiguous_prefix` for it | scion-core `error.rs` |
| Principal names | `github:` / `service:` followed by 1-255 printable ASCII bytes without `:` or `/` | scion-core `principal.rs` |
