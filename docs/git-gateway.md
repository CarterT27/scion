# Scion Git Gateway Specification (Addendum G)

Status: Draft, scoping. Extends `SPEC.md` (Draft v1). Section numbers prefixed `G` belong to this
document; bare section numbers refer to `SPEC.md`.

Purpose: Let unmodified git clients clone, fetch, and push against a Scion repository while keeping
every server-side property of Scion and without constraining the native `scion` client, daemon, or
RPCs.

Normative language follows `SPEC.md`. The git wire protocol, pack format, and object format are
external normative dependencies (git `Documentation/gitprotocol-v2.txt`,
`gitprotocol-http.txt`, `gitprotocol-pack.txt`, `gitformat-pack.txt`).

## G0. Summary

Git becomes a projection of Scion, not a peer of it:

- Git objects are derived data computed from Scion objects with a deterministic, frozen
  projection. Scion objects, identifiers, and formats do not change.
- A stateless `scion-git-gateway` speaks git smart HTTP. It plans fetches through new server
  RPCs and gets object bytes through the existing tiers, so the daemon cache serves git clients too.
- Pushes become ordinary changesets plus an identity binding that preserves the pushed commit ids.
  A push to `refs/for/<ref>` runs the lock-free `Land`, so git users get Scion landing semantics.
- Anything Scion can express but git cannot (name masking, redacted metadata, restricted
  subtrees) degrades the git view by omission or denial. It never limits the native view.

## G1. Design Principles

These rules settle every conflict in this document.

- **G-P1 Native first.** Nothing in this addendum changes canonical encoding, object identifiers,
  changeset validity, merge semantics, land semantics, or the meaning of any existing RPC.
  Additions are new derived types, new tables, new RPCs, new ref namespaces, and new
  configuration only.
- **G-P2 Off the native write path.** `UploadObjects`, `CreateChangeset`, `Rebase`, `Merge`, `Land`,
  and `UpdateRef` invoked by native clients perform no git hashing and no git projection work.
  Projection is lazy or asynchronous (G4.7).
- **G-P3 Degrade git, never native.** When a native feature has no git representation, the git
  projection omits, denies, or rejects. A native feature is never gated on git representability.
- **G-P4 Same authority, weaker guarantees.** A git request is evaluated as the authenticated
  principal under the same `acl.toml`. The gateway holds no privilege of its own. Git clients are
  uncontrolled caches: guarantees that depend on Scion controlling every copy (redaction
  propagation, name masking) do not extend to them, and this is documented, not hidden.
- **G-P5 Server-side scale is preserved.** The gateway is stateless and horizontally scalable.
  Fetch planning costs O(changed paths x depth) over the requested range, never O(repository),
  except for full clones, which are served from precomputed snapshot packs in object storage.
- **G-P6 Git identity is stable forever.** Once a git object id has been served, the bytes behind it
  never change. Projection rules are therefore frozen per repository (G19).

## G2. Goals, Non-Goals, Deferred

### G2.1 Goals

- `git clone`, `fetch`, `pull`, `ls-remote`, partial clone (`blob:none`, `blob:limit`, `tree:<n>`),
  sparse checkout, shallow fetch, and lazy promisor fetches against Scion over HTTPS.
- Byte-identical git object ids for history imported from git, and for commits pushed by git
  clients, so existing clones and links keep working after migration.
- Deterministic git ids for changesets created natively (CLI, API, server-side rebase/land).
- `git push` fast-forward updates to exposed refs, and `git push origin HEAD:refs/for/main`
  landing through `Land`.
- The same path and ref authorization, rate limits, and audit trail as native clients.
- Git reads served through the daemon tier when a node-local gateway is deployed.

### G2.2 Non-Goals

- Git as a source of truth, or two-way sync with an external git host (still Appendix A,
  "Bridge service").
- SSH transport, git protocol v0/v1 for fetch, dumb HTTP.
- Server-side git hooks, `git gc`/repack semantics, on-disk git repositories anywhere.
- Making git clients receive Scion guarantees that need control of every copy (G-P4).
- Git LFS, submodule contents.

### G2.3 Deferred

Annotated and lightweight tags, atomic multi-ref push, SSH, SHA-256 object format alongside SHA-1
in one repository, `sparse:oid` filters, server-signed native commits, git notes.

## G3. Architecture

### G3.1 Components

1. `scion-git-gateway` (new; stateless; horizontally scalable)
   - Terminates git smart HTTP (`/info/refs`, `/git-upload-pack`, `/git-receive-pack`).
   - Fetch: calls `GitListRefs` and `GitPlanFetch` on the server, gets object bytes with
     `GetGitObjects` from its upstream tier, and streams a pack.
   - Push: spools and indexes the incoming pack on local disk, verifies it, then calls
     `UploadObjects`, `GitIngestCommits`, and `UpdateRef`/`Land` as the pushing principal.
   - Holds no durable state. Local disk is only a push spool and an optional pack cache.

2. `scion-server` additions
   - `Git Projection` derived types and the `git_objects` index (G4, G5).
   - `Git Planner`: negotiation and object-set computation (G9).
   - `Git Ingest`: validates raw git commits and trees and creates bound changesets (G10).
   - `Seed Pack Job`: periodically writes snapshot packs for exposed refs to object storage (G9.6).

3. `scion-daemon` additions
   - Serves and caches `GetGitObjects` under the Section 7.8 tier conformance rule.

4. `scion` CLI additions
   - `scion git credential` (git credential helper protocol), `scion git setup` (remote, refspecs,
     partial clone, and commit-msg hook configuration), and `git:<oid>` revisions for any projected
     commit.

### G3.2 Topology

```text
 git ──HTTPS──▶ scion-git-gateway ──TLS gRPC──▶ scion-server ──▶ CockroachDB / object storage
                     │                              ▲
                     └──unix socket──▶ scion-daemon ┘   (node-local deployment)
 git ◀──HTTPS (packfile-uris / bundle-uri)── object storage / CDN (seed packs)
```

- Central deployment: gateways behind a load balancer use the server as their upstream.
- Node-local deployment: a gateway per node uses the local daemon as upstream. Read RPCs
  (`GetGitObjects`) are answered from the daemon cache. Planning, ref, and write RPCs are forwarded.
- A gateway talks only to its upstream tier, as in Section 3.3.

### G3.3 Layering

The projection lives in the Derived Layer. The planner and ingest live in the Service Layer. The
Object, Storage, and History layers gain no git dependency. Native code paths never call into
projection, planner, or ingest.

## G4. Git Projection

### G4.1 Object Format

- Each repository has one `git.object_format`: `sha1` (default, for client compatibility) or
  `sha256`. It is fixed when git support is enabled and never changes (G19).
- SHA-1 hashing MUST use collision-detecting SHA-1 (the SHAttered-detecting variant git uses).
  Objects that trigger collision detection are rejected on ingest.
- `H(type, body) = HASH(type || " " || decimal(len(body)) || 0x00 || body)`.

### G4.2 Blobs

`git_blob_oid(b) = H("blob", content(b))`. Content is identical in both systems, so the bytes
served for a git blob are the Scion blob bytes.

### G4.3 Trees

`git_tree(t)` maps each Scion tree entry to a git tree entry:

| Scion mode | Git mode text | Entry oid |
|---|---|---|
| 1 File | `100644` | `git_blob_oid(id)` |
| 2 Executable | `100755` | `git_blob_oid(id)` |
| 3 Symlink | `120000` | `git_blob_oid(id)` |
| 4 Tree | `40000` | `git_tree_oid(id)` |

- Entry encoding: `mode SP name 0x00 raw_oid`.
- Entries are sorted in git order: compare names bytewise, treating a tree entry's name as if
  followed by `/`. This differs from Scion's bytewise order (for example, file `a.txt` sorts before
  directory `a` in git and after it in Scion). The projection MUST re-sort.
- `git_tree_oid(t) = H("tree", body)`. The empty root tree projects to git's empty tree.
- Trees are keyed by Scion tree id, so identical subtrees anywhere in history project once.

Canonical git tree: a git tree whose entries use only the four modes above, contains no empty
subtree, no gitlink, no name Scion rejects (Section 4.4), and is in git order. For every canonical
git tree `G`, `git_tree(parse(G))` MUST equal `G` byte for byte. Non-canonical trees are stored raw
(G4.6).

### G4.4 Commits

Each changeset `C` has exactly one git commit id, `git_commit_oid(C)`, chosen by this rule:

1. **Bound.** If `git_mapping` has an entry for `C` (written by the importer or by push ingest
   *before* `C` was created), `git_commit_oid(C)` is that oid and the commit bytes are the stored
   raw commit (G4.6).
2. **Native.** Otherwise the commit is serialized by `scion-git-commit-v1`:

```text
tree <git_tree_oid(C.root_tree)>
parent <git_commit_oid(P)>              one line per parent, in order
author <ident(Meta.author)> <secs> <tz>
committer <ident(Meta.committer or Meta.author)> <secs> <tz>
scion-change-id <32 hex of C.change_id>

<Meta.message, verbatim>
```

- Times come from `Meta.author_time` and from `Meta.committer_time` (falling back to
  `author_time`). `tz` is `+HHMM`/`-HHMM` from `tz_offset_minutes`.
- `ident(x)`: if `x` matches `^[^<>\n]* <[^<>\n]*>$`, use it verbatim. Otherwise use `x` with
  every `<`, `>`, and `\n` replaced by `?`, followed by ` <>`. This rule is deterministic and never
  rejects.
- The changeset id is deliberately absent, so git ids survive a future Scion hash-algorithm
  change (Appendix C).
- No wall-clock time enters the projection. Rebase stays a pure function, and its result has a
  deterministic git id.
- A rebased or landed copy of a bound changeset is a new changeset without a `git_mapping` entry, so
  it projects natively. Signatures on the original pushed commit are not carried over.

### G4.5 Determinism and Freezing

- `git_blob_oid`, `git_tree_oid`, and `git_commit_oid` are pure functions of Scion objects plus the
  `git_mapping` binding. The binding is written once and never rewritten.
- The projection version (`scion-git-commit-v1`, the tree rules, `object_format`) is recorded per
  repository at enablement and MUST NOT change while any of its ids has been served. A new
  projection version may only apply to a new repository.

### G4.6 Raw Git Objects

Some git objects cannot be regenerated from Scion objects. They are stored verbatim in
`git_raw_objects` (G5):

- Commits that git clients pushed, and imported commits (bound commits, G4.4), so ids stay
  byte-exact, including `gpgsig`, `mergetag`, `encoding`, and unknown headers.
- Non-canonical trees from import or push: odd modes such as `100664`, empty subtrees, gitlinks.
  The Scion changeset holds the canonicalized tree (as Section 14 does today). The raw tree keeps
  the git id chain intact.

Raw commits are content-bearing with respect to their Meta: each row records `meta_id`, and Meta
redaction deletes it (G12).

### G4.7 Projection Triggering and Cost

- Derived types `git_blob`, `git_tree`, and `git_commit` are `Lazy` (Section 9.1). Any request that
  needs a git id derives it forward from the nearest projected ancestor, iteratively, with leases
  only for de-duplication.
- The `git_projector` background job follows `ref_log` for exposed refs (G8) and projects new
  targets shortly after they land, so ref advertisement rarely pays projection latency. It is an
  optimization: correctness never depends on it.
- Cost per changeset: O(|delta| x depth) new git trees, plus one hash pass per new blob. Blob
  hashing reads blob bytes, so the projector streams blobs through the normal object path and
  caches.
- Importer and push ingest already know the git oids of the blobs and trees they write, and record
  them directly (no re-hash).
- Redaction MUST materialize `git_blob` for each redacted blob, and `git_commit` for each changeset
  whose Meta is redacted, before the quarantine transaction commits (G12). Otherwise the oid could
  never be computed after purge, and every descendant commit would become unprojectable.

## G5. Storage Additions

| Table | Primary key | Columns |
|---|---|---|
| `git_objects` | `(repo_id, git_oid)` | `kind` (`blob`/`tree`/`commit`), `scion_id?`, `raw` (bool), `size` |
| `git_raw_objects` | `(repo_id, git_oid)` | `kind`, `meta_id?`, `changeset_id?`, `inline_data?`, `storage_key?` |
| `git_seed_packs` | `(repo_id, ref, git_oid, filter)` | `storage_key`, `pack_hash`, `object_count`, `created_at` |

- `git_objects` is the git-id index used for `want` lookups, lazy promisor fetches, and `have`
  resolution. Rows are written idempotently (`ON CONFLICT DO NOTHING`) whenever an object is
  projected, imported, or ingested.
- The reverse direction (Scion id to git id) uses the `derived` table, keyed by Scion id.
- `git_mapping` (Section 9.2) keeps its meaning: an immutable binding for imported and pushed
  commits. It becomes the "Bound" source of G4.4.
- Large raw objects use object storage under kind letter `g`. No existing key layout changes.
- Secondary index `changesets(meta_id)` (allowed by Section 5.2) supports Meta-redaction
  materialization.
- Nothing here participates in Scion object identity.

## G6. Configuration

### G6.1 Server (`scion-server.toml`)

```toml
[git]
enabled = true
object_format = "sha1"              # fixed at enablement
exposed_refs = ["main", "release/*"]
head = "main"
projector_lag_target_ms = 2000
seed_pack_interval = "1h"
seed_pack_filters = ["", "blob:none"]
user_refs = true                    # G8.3
```

### G6.2 Gateway (`scion-git-gateway.toml`)

```toml
listen = "0.0.0.0:8443"
tls_cert = "/etc/scion/git-tls.crt"
tls_key = "/etc/scion/git-tls.key"
upstream = "unix:///run/scion/daemon.sock"   # or https://scion-server
spool_dir = "/var/lib/scion-git/spool"
max_push_bytes = 2147483648
pack_cache_dir = "/var/cache/scion-git"      # optional
pack_cache_max_bytes = 21474836480
packfile_uri_base = "https://scion-packs.example.internal/"
require_filter_for_unfiltered_closure_gaps = true   # G9.5
```

### G6.3 Access Policy (`acl.toml`) Additions

```toml
[git]
read = ["group:eng"]              # who may fetch via the gateway (subset of path read)
write = ["group:eng"]             # who may push via the gateway (subset of path write)

[[paths]]
prefix = "/secrets"
read = ["group:security"]
write = ["group:security"]
git = false                       # never served to git clients, whatever the read rule says
```

- `[git]` is an additional gate on gateway RPCs. It never grants anything `[[paths]]` and
  `[refs]` deny.
- `git = false` on a path rule makes objects whose every `object_paths` path falls under such rules
  unservable through git RPCs. This is a data-hygiene control that keeps content out of
  uncontrolled clones, not a security boundary: the same principal can still read through native
  RPCs.
- Both fields are optional. Their absence means no additional restriction. Adding them changes
  `policy_epoch` as usual.

## G7. RPC Additions (`scion.v1`, additive)

### G7.1 Object RPCs (served by every tier)

- `GetGitObjects(oids) -> stream {oid, status, kind, size, data, final}`
  - `data` is the git object body (no header). The receiver MUST verify `H(kind, data) == oid`
    before using or caching it.
  - `status` per item: `ok`, `not_found`, `denied`, `redacted`, `unprojectable`.
  - Blob items are the Scion blob bytes. Tree and commit items are generated by the server from the
    projection, or read from `git_raw_objects`.
  - Authorization: blobs and trees inherit the underlying Scion object's authorization and
    `access_class`. Commits are readable by every repository reader (consistent with Section 12.6
    Meta visibility) and have `access_class = root`.
  - Unauthorized and nonexistent oids both return `not_found`, the git analogue of `HasObjects`
    probe protection.
  - The daemon caches items by git oid with the same epoch, redaction, and `CheckAccess` rules as
    `GetObjects` (Sections 10.2-10.4).

### G7.2 Query RPCs (server; forwarded by daemon)

- `GitListRefs(prefixes, include_symrefs) -> list<{git_name, oid, scion_ref?, version?, symref?}>`
  - Resolves exposed refs, user refs (G8.3), and virtual refs (G8.4), projecting tips on demand.
- `GitPlanFetch({wants, want_refs, haves, filter, shallow, deepen, deepen_since, deepen_not,
  done}) -> stream {acks | shallow_info | wanted_refs | object_batch{oids, kinds, sizes,
  delta_base_hints} | seed_packs{uri, pack_hash} | final}`
  - Implements G9.2-G9.5 and returns the object set; the gateway fetches bytes separately.
- `GitResolve(oids | changesets) -> per-item mapping`

### G7.3 Write RPCs (server)

- `GitIngestCommits(stream {raw_commits, raw_trees, blob_bindings}) -> per-commit
  {git_oid, changeset_id, status}`
  - Blobs are uploaded first through `UploadObjects`, and `blob_bindings` pairs each git blob oid
    with its Scion blob id. The server recomputes every git oid itself and does not trust the
    gateway.
  - Algorithm and validation: G10.2.

### G7.4 Errors

These map existing `ErrorDetail` codes to git: `PERMISSION_DENIED` and `UNAUTHENTICATED` become
HTTP 403/401. Others become `ERR <message>` packets on fetch, or per-ref `ng <ref> <reason>` on push.
`REF_VERSION_MISMATCH` during a direct push is reported as `ng <ref> fetch first`, matching git
users' expectations.

## G8. Ref Mapping

### G8.1 Exposed Refs

- Each Scion ref matching `git.exposed_refs` appears as `refs/heads/<name>`.
- `HEAD` is a symref to `refs/heads/<git.head>`.
- Ref reads are strongly consistent (Section 5.4). The advertised oid is
  `git_commit_oid(target)`, projected on demand when the projector lags.

### G8.2 Direct Push

A push to `refs/heads/<name>` for an exposed ref performs `UpdateRef(name, expected_version,
new_target)` with the version read during advertisement. It requires `update` (and `force` for
non-fast-forward with `+`). Ref creation and deletion require `admin.refs`.

### G8.3 User Refs (feature branches)

Git workflows need pushable branches, while Scion discourages shared refs. Proposed additive
namespace:

- Git `refs/heads/u/<login>/<branch>` maps to Scion ref `user/<login>/<branch>`.
- For refs under `user/`, the Section 4.3.6 grammar is widened to git's `check-ref-format` subset
  (`[A-Za-z0-9._/-]`, git's other rules applied). Other namespaces keep the v1 grammar.
- New ACL principal token `owner`: in `[refs."user/*"]`, `update = ["owner"]` and
  `force = ["owner"]` grant the principal whose login equals the `<login>` segment. Creating or
  deleting one's own user refs needs no `admin.refs`.
- Advertisement: an unprefixed `ls-refs` lists only the caller's own `u/<login>/` refs. Other users'
  refs are listed only for an explicit `ref-prefix refs/heads/u/<other>/`. This prevents ref sprawl
  in default fetch refspecs.
- Native clients may use `user/*` refs too. This is an addition for them, not a limit.

### G8.4 Virtual Refs

- `refs/scion/c/<changeset id text form>` resolves to any changeset the caller can read. These refs
  are never listed. They resolve only for an exact `ref-prefix` or `want-ref`. This lets git users
  fetch API-created or draft changesets (`git fetch origin refs/scion/c/c1...`) without creating
  refs.
- `refs/replace/<oid>` entries for redacted commits (G12.2) are listed only under the
  `refs/replace/` prefix.

### G8.5 Tags

Not supported in this addendum. Imported tags are not advertised (G2.3).

## G9. Read Path (upload-pack, protocol v2)

### G9.1 Capability Advertisement

`agent`, `ls-refs=unborn`, `fetch=shallow wait-for-done filter ref-in-want sideband-all
packfile-uris`, `server-option`, `object-format=<git.object_format>`, `session-id`,
`object-info`, `bundle-uri` (optional). Requests without `Git-Protocol: version=2` are rejected with
a message naming the minimum supported git version (2.30; packfile-uris and bundle-uri need newer
clients and are optional).

Any authorized oid may be wanted, which is the equivalent of `uploadpack.allowAnySHA1InWant`. This
is how lazy promisor fetches of individual blobs and trees work.

### G9.2 Negotiation

- Each `have` is resolved through `git_objects`. Unknown or unauthorized haves are ignored, without
  revealing which.
- The common set is computed with `commit_graph` generations and skip pointers (`IsAncestor`), not
  by walking full history. The server sends `ready` as soon as a set of common haves covers every
  want's ancestry frontier, or when the client sends `done`.
- Shallow requests use generation numbers for `deepen <n>`, and committer time from the projected
  commit for `deepen-since`. `deepen-not` resolves the named ref and excludes its ancestors.

### G9.3 Object Set Algorithm

Let `R` be the changesets reachable from the wants and not from the common haves (bounded by
shallow limits). The pack contains:

```text
objects = { git_commit_oid(C) : C in R }
        ∪ ⋃_{C in R} new_git_objects(C)
new_git_objects(C) = git trees for every directory on a path in C.delta (in C's tree)
                   ∪ git_blob_oid(e.id) for every Put e in C.delta
                   ∪ (for parentless C) every object in C's root tree
                   ∪ raw trees of bound commits whose ids differ from projection
```

Completeness argument: every object in `closure(tree(C))` either equals the object at the same
place in `tree(C.parents[0])`, or lies on a delta path of `C`. By induction along first parents
down to a common have (whose closure the client holds or has promised) or to a parentless
changeset, `closure(wants) \ closure(haves) ⊆ objects`. Sending extra objects (for example,
objects reintroduced by a revert or reachable through a second parent) is allowed. Git
de-duplicates within a pack and against its store.

Cost: O(Σ_{C in R} |C.delta| x depth). Fetch cost is proportional to the change since the client's
last fetch, not to repository size. This is Scion's write-side property carried into git reads.

### G9.4 Filters

- `blob:none`: trees and commits only.
- `blob:limit=<n>`: blob sizes come from Scion tree entries (`size`), so filtering reads no blob
  bytes.
- `tree:<depth>`: trees deeper than `depth` from the root are omitted.
- `object-info` answers sizes from tree entries without fetching blobs.
- `sparse:oid` is deferred.

### G9.5 Omission Rule (the git-side degradation mechanism)

Some objects in the computed set cannot be sent to this principal: denied, `git = false`,
redacted, or unprojectable.

- **Filtered fetch** (any `filter`): omit them. Git treats objects missing from a promisor pack as
  promised. A later lazy fetch of one returns `not_found`, so the git operation that needs it fails
  for that path only. This is the git analogue of Scion's `inaccessible` paths and redaction
  placeholders.
- **Unfiltered fetch**: git requires a complete closure, so omission is impossible. The fetch fails
  with an `ERR` naming the cause and the remedy (`git clone --filter=blob:none`, or the native
  client), unless the principal can read every object in the set.
- `scion git setup` configures partial clone by default, so most users never hit the second case.

### G9.6 Pack Generation and Bulk Transfer

- The gateway streams object bytes from `GetGitObjects` (daemon cache first) into the pack.
- Default packs are undeltified: zlib per object, OFS_DELTA unused.
- RECOMMENDED delta hints: for each Put blob, the planner names the previous blob at the same path
  in the first parent as a `delta_base_hint`. When that base is in the client's have closure, the
  gateway emits a REF_DELTA thin-pack entry. Cached deltas are a content-bearing derived type keyed
  by both blob ids, so redaction purges them (Section 9.1).
- Seed packs: the `Seed Pack Job` writes snapshot packs for each exposed ref and each configured
  filter to object storage. Clones and large fetches receive these through `packfile-uris`
  (protocol v2) or `bundle-uri`, plus an incremental pack computed by G9.3 from the seed's commit.
  Object storage and CDNs then serve the bulk bytes. No server builds full-repository packs on
  demand.
- Seed packs contain only objects with `access_class = root` and no `git = false` paths. Anything
  restricted is fetched per-principal through the incremental pack or lazily.
- Pack cache: incremental packs are cacheable under
  `BLAKE3(object_set, filter, capabilities)` and served as `packfile-uris`. This gives CI fleets
  fetching the same `main` advance a high hit rate.

## G10. Write Path (receive-pack)

Git push uses protocol v0/v1 receive-pack even when fetch uses v2. The gateway implements
receive-pack over smart HTTP with `report-status`, `report-status-v2`, `side-band-64k`,
`push-options`, `ofs-delta`, `delete-refs`, `quiet`, and `object-format`. It does not advertise
`atomic` (G2.3).

### G10.1 Gateway Steps

1. Authenticate. Check `[git] write`. Spool the pack (bounded by `max_push_bytes`), resolve thin-pack
   bases through `GetGitObjects`, and verify every object id with collision-detecting hashing.
2. Upload new blobs with `UploadObjects` (BLAKE3 ids, as for any native upload).
3. Call `GitIngestCommits` with the new commits in topological order (parents first), their new
   trees, and blob bindings.
4. Apply ref commands (G10.3, G10.4) and report status per ref, with sideband messages for landed
   results.

### G10.2 Ingest (server)

For each commit, parents first:

- Every parent oid MUST resolve to a changeset, either from this push or through `git_objects`.
  Otherwise reject `missing parent`.
- Parse the root tree recursively, using raw trees from the push or known projected or raw trees.
  Reject the commit when any reachable file:
  - violates Section 4.4 name or path rules (including `.scion`);
  - exceeds `limits.max_blob_bytes`;
  - would need more than `limits.max_delta_entries` delta entries.
- Gitlinks and empty subtrees are dropped from the Scion tree (as in Section 14). Each non-canonical
  tree is stored in `git_raw_objects`.
- `delta = FileDiff(first parent's Scion tree, canonicalized commit tree)`. Merge commits are
  accepted for direct pushes; they are ordinary changesets.
- Meta: author, committer, both times and zones, message, and extension tag 2 = commit oid (as the
  importer does).
- `change_id`, in order of preference: a `scion-change-id` header; else a `Scion-Change-Id:` message
  trailer (survives `git commit --amend`, inserted by the hook `scion git setup` installs); else
  `BLAKE3("scion-git-push-v1" || commit_oid)[0:16]`.
- Write order: the raw commit and the `git_mapping` binding first, then `CreateChangeset` with
  `expected_root_tree`, then `git_objects` rows. A binding without a changeset is a harmless
  orphan. Writing the binding first guarantees the changeset is never projected natively (G4.4).
- Authorization: write on every delta path (Section 12.4). `CreateChangeset` performs this
  unchanged.
- Idempotency: re-pushing the same commit yields the same changeset (the Meta carries the oid, so
  the changeset id is deterministic).

### G10.3 Direct Ref Updates

`refs/heads/<exposed or user ref>` updates run `UpdateRef` (G8.2, G8.3). The pushed commit becomes
the ref target, so the user's local oid matches the remote exactly. Deletions use `DeleteRef`.

### G10.4 Landing (`refs/for/<ref>`)

`git push origin HEAD:refs/for/main` runs `Land(main, head)`:

- Scion semantics apply: lock-free rebase and CAS, idempotent retries, `landed` index, and the
  single-parent stack requirement. Stacks containing merge commits are rejected with a hint to
  rebase.
- The client sees `ok refs/for/main` plus sideband lines
  `landed <git oid> as main@<version>`. The landed commits are rebased native projections (G4.4),
  so `git pull --rebase` later drops the local copies as already upstream.
- Mapping: `MERGE_CONFLICT` becomes `ng refs/for/main conflict: <paths>`, `ALREADY_LANDED` becomes
  `ng ... already landed`, and `EMPTY_CHANGE` becomes `ng ... no changes` (unless `-o
  scion.allow-empty`).
- Push options: `-o scion.allow-empty`. Further land policy hooks stay deferred.

### G10.5 Pushes Never Bypass Scion Validation

Everything a git push creates goes through `UploadObjects` and `CreateChangeset` validation and
authorization. Ingest adds checks but removes none.

## G11. Authentication and Access Control

- Transport credentials: HTTP Basic with any username and a Scion token as the password, or
  `Authorization: Bearer`. Tokens are verified exactly as in Section 12.1. The gateway forwards the
  user's token upstream: it never acts as its own principal.
- `scion git credential` implements git's credential-helper protocol. It returns a cached token and
  refreshes it through the stored GitHub token or service secret (Sections 12.2, 12.3).
- Every git RPC is authorized as the principal. Per-object rules come from `object_paths`
  (Section 12.5) through `git_objects.scion_id`. Raw trees are authorized by the path recorded at
  ingest or import.
- History filtering (Section 12.6) has no git equivalent. A git commit is all-or-nothing, so commits
  are readable by repository readers and per-path filtering happens at tree and blob granularity
  through the omission rule (G9.5).
- Tree names of restricted subdirectories are visible in readable parent trees, which already
  applies to native clients (Appendix A). If serve-time name masking is built, every tree containing
  a masked name becomes `denied` for git (G9.5). Masking never waits on git.
- Rate limits are per principal and shared with native RPCs (Section 10.5).

## G12. Redaction Interplay

### G12.1 Blobs

- Before quarantine, `Redact` materializes `git_blob` for the ids (G4.7). Tree projections stay
  computable forever.
- `GetGitObjects` returns `redacted`. Filtered fetches omit the blob, and lazy fetches of it fail.
  Unfiltered fetches whose closure includes it fail (G9.5).
- Cached deltas involving the blob are purged with it (content-bearing).
- Git clones that already hold the blob keep it. Section 13.3 propagation bounds cover Scion tiers
  only (G-P4). Operator runbooks MUST say so.

### G12.2 Meta

- Before quarantine, `Redact` materializes `git_commit` for every changeset using the Meta (through
  the `changesets(meta_id)` index). Descendant commit ids therefore stay computable.
- Purge deletes `git_raw_objects` rows with that `meta_id`. The commit's oid stays in `git_objects`,
  and `GetGitObjects` returns `redacted`.
- RECOMMENDED: the server synthesizes a substitute commit with the same tree and parents, author
  `redacted <>`, and message `<redacted>\n`. It is advertised as `refs/replace/<oid>` (G8.4).
  `scion git setup` adds `+refs/replace/*:refs/replace/*` to the fetch refspec. Git honors replace
  refs by default (`core.useReplaceRefs`), so `log` and `blame` work across the redacted commit.

### G12.3 Unprojectable History

A blob purged *before* git was enabled, on a native-only path with no recorded git oid, makes every
tree containing it, and every commit above those trees, unprojectable. `scion git preflight` MUST
report such blobs before enablement. Affected history is served only to native clients.

## G13. Native Non-Interference Guarantees

| Native property | Effect of this addendum |
|---|---|
| Object formats, ids, canonical encoding | Unchanged |
| `CreateChangeset` / `Rebase` / `Merge` / `Land` / `UpdateRef` latency and cost | Unchanged: no git work on these paths (G-P2). Complexity gate in G16 |
| Daemon cache for native RPCs | Unchanged. Git items use separate keys and share eviction |
| Fetch walk, sparse, status, stat cache | Unchanged |
| Merge engine and future engine versions | Unchanged. Results are changesets, and git ids follow |
| Hash-algorithm migration (Appendix C) | Unaffected: git ids do not include Scion ids |
| Chunked large blobs | Unaffected: the gateway assembles whole blobs for git |
| Sharded huge directories (future tree format) | Unaffected: projected into one git tree |
| Path-name masking, per-changeset Meta visibility | Native-only. Git sees `denied` (G11) |
| Refs | Additive `user/*` namespace and virtual refs only |
| Redaction | Stronger requirement only: oids are materialized before quarantine |

Rule for future work: a feature proposal that would need to change native behavior to fit git is
rejected or redesigned. A proposal that git cannot represent ships native-only and declares its git
degradation (omit, deny, or reject).

## G14. Failure Model

- Gateway crash mid-fetch: the client retries. The gateway holds no state.
- Gateway crash mid-push: spooled data is lost and the client retries. Objects, bindings, and
  changesets already written are idempotent orphans or reusable. No ref moved unless `UpdateRef` or
  `Land` committed.
- Projector lag: advertisement projects on demand. Lag shows up as latency only, never
  incorrectness.
- Projection or ingest bug: projection is deterministic and verified by golden vectors (G16). A
  mismatch between a raw object and its projection is a server bug surfaced as `INTEGRITY`.
- Object storage outage: seed packs are unavailable, so clones fall back to slower full pack
  generation, or are shed with `RESOURCE_EXHAUSTED` when over budget.

## G15. Observability

Metrics:

- `scion_git_requests_total{service,status}`
- `scion_git_pack_bytes_total{source=gateway|seed|cache}`
- `scion_git_plan_changesets` (histogram of |R|)
- `scion_git_projection_lag_ms{ref}`
- `scion_git_projections_total{type,mode=eager|ondemand}`
- `scion_git_omitted_objects_total{reason}`
- `scion_git_push_total{kind=direct|land,result}`
- `scion_git_ingest_rejections_total{reason}`

Logs carry the principal, git `session-id`, request id, wants/haves counts, and the size of the
computed object set.

## G16. Tests and Conformance

- **Projection golden vectors.** Git-order sorting (`a.txt` vs `a/`), every mode, the empty tree,
  ident sanitization, timezones, non-UTF-8 messages, and multi-parent commits. For canonical trees,
  `project(parse(G)) == G` holds as a property test.
- **Byte-identical round trip.** Import a real repository (the `pingdotgg/t3code` benchmark history
  and the Section 19.14 corpora), clone through the gateway, and check:
  - `git rev-parse` of every exposed ref equals the source repository's;
  - `git fsck --strict` passes;
  - both full and `blob:none` clones match.
  Repeat with repositories containing `100664` modes, empty trees, gitlinks, and signed commits.
- **Differential views.** For every exposed changeset, `git ls-tree -r` from a gateway clone equals
  Scion `ListTree` recursion (modulo dropped gitlinks and empty trees), and file bytes match.
- **Object-set completeness.** Randomized histories (merges, reverts, deletes and re-adds, parentless
  imports) with random have sets. The pack plus the have closure contains the want closure. Checked
  with `git fsck` and `rev-list --objects --missing=error`.
- **Native non-regression and complexity gate.** The full Section 19 suite passes with git enabled.
  An instrumented hasher shows zero SHA-1 or projection calls during native `CreateChangeset`,
  `Rebase`, `Land`, and `UpdateRef`.
- **Authorization.**
  - Denied, `git = false`, and redacted objects are omitted in filtered fetches and fail unfiltered
    fetches.
  - Unauthorized wants and haves are indistinguishable from nonexistent ones.
  - `[git]` gates are enforced.
  - Policy reload takes effect on the next request.
- **Redaction.** Oids are materialized before quarantine, `redacted` statuses are returned, replace
  refs work, and purge deletes raw commits and deltas.
- **Push.**
  - Fast-forward and non-fast-forward handling, CAS races between concurrent pushers, and direct
    pushes of merge commits.
  - `refs/for/main` landing, conflict reporting, `ALREADY_LANDED`, and idempotent re-push.
  - Thin packs, `.scion`-named paths rejected, over-limit blobs rejected.
  - Change-id precedence between header, trailer, and derived id.
  - User-ref ownership.
- **Tier conformance.** `GetGitObjects` via daemon is byte-identical to the server for the same
  principal and epochs (Section 7.8).
- **Client matrix.** Git 2.30 (minimum) and current release, Linux and macOS: clone, fetch, lazy
  fetch, sparse checkout, push. Packfile-uris and bundle-uri are tested where the client supports
  them.

## G17. Phasing

| Phase | Scope | Exit criteria |
|---|---|---|
| G-0 Projection core | `git_blob`/`git_tree`/`git_commit` derived types, `git_objects`, `git_raw_objects`, importer records blob, tree, and raw commit data, `GitResolve`, `git:<oid>` revisions, `scion git preflight` | Golden vectors pass. Re-imported t3code history projects every commit to its original GitHub oid. Native complexity gate holds |
| G-1 Read-only gateway | Smart HTTP v2 `ls-refs` and `fetch`, G9.2-G9.5 planner, undeltified packs, filters, shallow, token auth and credential helper, `GetGitObjects` on server | `git clone` (full and `blob:none --sparse`) and incremental fetch match source oids. Completeness property test passes |
| G-2 Scale | Seed packs with packfile-uris/bundle-uri, pack cache, node-local gateway over daemon, delta hints | Clone-time and server-CPU benchmarks against plain git (extending `scripts/benchmark-git.py`). Incremental fetch cost tracks |R|, not repository size |
| G-3 Writes | receive-pack, ingest, direct push, `refs/for/<ref>` land, user refs, change-id hook | Push and land tests pass. Concurrent pushers never lose updates. Pushed oids preserved |
| G-4 Policy and redaction | `[git]` gates, `git = false`, materialize-before-quarantine, replace refs, virtual changeset refs | Redaction and authorization suites pass. Runbook documents the uncontrolled-clone caveat |

G-0 alone is valuable: it gives every changeset a stable git id for external tools, review links,
and differential testing. G-0 and G-1 together already let any git client read Scion.

## G18. Changes to `SPEC.md`

- **§2.2:** replace "Git wire-protocol or on-disk compatibility" with "On-disk git compatibility;
  git as a source of truth". Add a pointer to this addendum.
- **§2.3 / Appendix A:** "Two-way git sync" stays deferred. Add the G2.3 items.
- **§4.3.6:** widened grammar for `user/*` refs. Add the `refs/scion/c/*` virtual-ref note.
- **§5.2:** add the G5 tables and the `changesets(meta_id)` index.
- **§6:** add `[git]` server keys, the gateway config, and the `acl.toml` `[git]`, `git =`, and
  `owner` additions.
- **§7:** add the G7 RPCs. `GetGitObjects` joins the Section 7.3 tier-served set.
- **§9.2:** add the `git_blob`, `git_tree`, and `git_commit` lazy types. Restate `git_mapping` as
  the binding table written by the importer and by push ingest.
- **§11.8:** `git:<oid>` resolves any projected commit, not only imported ones.
- **§13.2:** materialize git oids before quarantine. Purge deletes `git_raw_objects` by `meta_id`.
- **§14:** the importer records git blob and tree oids, raw commits, and non-canonical raw trees.
- **§19 / §20:** add G16 as an Extension Conformance profile.
- **Appendix B:**
  - D-13: git as a frozen, lazy projection over Scion objects.
  - D-14: omission rule with partial clone as the degradation mechanism.
  - D-15: `refs/for/<ref>` maps to `Land`.

## G19. One-Way Doors and Open Questions

| Commitment | Why it is irreversible or bounded |
|---|---|
| `git.object_format` per repo | Irreversible for that repository's git clients (git's own transition story). SHA-256 interop is deferred |
| `scion-git-commit-v1` and the tree projection | Frozen once any id is served. A change requires a new repository or a full git-side history rewrite |
| `scion-change-id` header in native commits | Part of the frozen projection. Chosen so git users can track changes across rebase |
| Binding written before changeset creation | Ensures each changeset has exactly one git id |
| `user/*` ref namespace and `owner` principal | Additive. Can be disabled with `git.user_refs = false` without affecting existing refs |

Open questions:

1. Should committer time of native commits stay deterministic (author time), or reflect land
   time? Land time would make rebase impure, so the current answer is to keep it deterministic and
   leave land time in the ref log.
2. Should Section 19 cover the minimum git version, or a rolling "current minus N releases"
   window?
3. Should `refs/for/<ref>` also accept merge commits, once Appendix A "merge changesets in land
   stacks" is built?
4. Should seed packs be per-filter only, or also per top-level directory, to help sparse clones
   whose cone covers a small part of the tree?
