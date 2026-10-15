# Scion

Source control for always-online clients, coding agents, and CI.

Scion is an independent, clean-room Rust implementation inspired by Anthropic's
[Redesigning source control](https://www.anthropic.com/engineering/redesigning-source-control).
It explores the architecture described in that post: keep history and expensive operations
on the server, fetch immutable objects as needed, and check out only the paths a job needs.
This repository is not Anthropic's implementation and is not affiliated with Anthropic.

## Benchmarks

The [benchmark runner](crates/scion-conformance/examples/benchmark.rs) compares Scion with
Git on the four synthetic workloads used by
[OpenScion](https://github.com/tinyhumansai/openscion/blob/main/benchmarks/README.md).
These are local measurements of this implementation, using its own deterministic fixture.

Median wall-clock times, measured 2026-10-14 on an Apple M4 Pro with 24 GiB RAM,
macOS 26.6.2, Rust 1.98.0, and Git 2.56.0. Scion was built in release mode from
`9d27736` plus the benchmark runner. Each file contains 4 KiB of seeded pseudorandom bytes,
spread across 10 directories; the 1,000-file case uses seven runs and the 10,000-file case
uses five. All times are milliseconds; lower is better.

| Scenario | Git, 1k files | Scion, 1k | Git, 10k files | Scion, 10k |
| --- | ---: | ---: | ---: | ---: |
| Import directory | 456.6 | 39.7 | 3,907.1 | 500.5 |
| Commit one changed file | 33.6 | 36.8 | 166.9 | 203.6 |
| Full clone / checkout | 242.2 | 170.1 | 2,223.9 | 1,590.2 |
| One-directory clone / checkout | 89.8 | 29.8 | 159.7 | 173.2 |

Scion imports and materializes the full tree faster in this setup. Git commits faster at
both sizes and is slightly faster for the 10k-file sparse case. Sparse checkout materializes
100 or 1,000 files respectively and fetches exactly that many blobs plus two trees.

Reproduce the measurements (requires Git and the build prerequisites below):

```sh
cargo run --release -p scion-conformance --example benchmark -- 100 7
cargo run --release -p scion-conformance --example benchmark -- 1000 5
# Quick correctness check: 10 directories × 2 files, one run
cargo run --release -p scion-conformance --example benchmark -- 2 1
```

The positional arguments are files per directory and run count (defaults: `100 5`).
The runner prints each sample to stderr and a median table to stdout, uses a fresh temporary
repository and server per run, and removes its scratch files afterward. Setup, fixture
generation, authentication, and byte-for-byte checkout verification are outside the timers.

- **Import:** Git runs `init`, `add -A`, and `commit`. Scion reads the fixture files, hashes
  and uploads blobs in batches of 256, creates a parentless changeset, and creates a ref
  through the write APIs. This measures directory ingestion, not `scion-import`'s Git-history
  migration; fixture path enumeration is prepared beforehand.
- **Commit:** Change one byte in one file of the full working copy, then time Git
  `commit -a` or Scion's working-copy commit, including status scanning and blob upload.
  Neither side includes a subsequent push or land.
- **Full checkout:** Git uses `clone --no-local` to build and transfer a pack; Scion uses
  `WorkingCopy::checkout` with its default 256-object fetch batches.
- **Sparse checkout:** Git uses `clone --no-local --filter=blob:none --sparse` followed by
  `sparse-checkout set dir0000`, with filtering enabled on the source. Scion checks out
  only `dir0000`. Both outputs are checked against the fixture, including absent paths
  outside the selected directory.

These measurements use an in-process Scion server and client over real loopback TLS/gRPC,
with in-memory metadata and object storage, elevated test rate limits, and no daemon.
Scion still performs authorization, BLAKE3 verification, and production zstd compression,
but does not pay CockroachDB/S3 latency or durable server writes. Git uses disk-backed
storage, compression, packing, and subprocesses; Scion's CLI process startup is excluded.
OS caches are not flushed, and recently written working copies can trigger timestamp-related
rehashing during commit. At 10k files, Git's commit samples ranged from 67.5 to 253.8 ms.
These are small, single-client, single-commit workloads, not production throughput results
or a direct comparison with OpenScion's published timings.

### Real repository with persistent storage

The [CLI benchmark](scripts/benchmark-git.py) also exercises the full default-branch history
of `pingdotgg/t3code` at `bf8ffe81a63c82d3de5fbaa9991fd5f30034a452`: 5,573 commits and
28,068 materialized files. These 2026-10-14 measurements use release binaries, local HTTPS,
and Docker CockroachDB/MinIO with the default permissions, verification, and rate limits.
Submodule contents are omitted on both sides.

Median seconds, with three fresh-directory checkout trials after a separate first measurement:

| Checkout | Scion before | Scion after | Less elapsed time |
| --- | ---: | ---: | ---: |
| Full tree, direct server | 30.43 | 14.49 | 52% |
| Full tree, warmed daemon | 18.62 | 10.19 | 45% |
| `apps/web`, direct server (1,521 files) | 2.28 | 0.95 | 58% |
| `apps/web`, warmed daemon | 0.48 | 0.45 | 6% |

Git's full-tree checkout was 2.47 seconds in the direct comparison. In the expanded
resource-instrumented run, Git took 3.03 seconds for the full tree and 0.21 seconds for
`apps/web`. Compare before and after within a row: direct full checkout repeats the original
procedure, while daemon and sparse workloads use the expanded runner. Back-to-back controls
showed no material change in status, history, or single-file-read time. OS caches are not
flushed; these are single-client local measurements, not controlled cold-cache or WAN tests.

A final run against a freshly imported store measured 14.96 seconds for direct full checkout
versus Git's 2.90 seconds, and 0.97 seconds for sparse checkout versus Git's 0.21 seconds.
Every checkout passed hash and mode checks. The complete release import into an empty
CockroachDB database and MinIO prefix took 1,397.59 seconds (23m18s), importing all 5,573
commits and 84,696 blobs with the same final graph identifier as the original import.
The local Git full-history clone without checkout took 3.56 seconds; Scion import also
constructs and durably publishes server history, so these operations have different costs.
The earlier full import used a development build and is not a controlled before/after
timing. Reimporting unchanged history uploaded nothing (0.060 seconds versus Git fetch's
0.031 seconds). No-change status, history, and single-file reads remain broadly unchanged.

The expanded runner records every sample, client CPU and peak RSS on macOS, sampled server
CPU/RSS, RPC and byte counters, and logical metadata calls/object-storage requests when the
server exports them. It verifies checkout file hashes and executable/symlink modes outside
the timers, checks matching sparse scopes, and removes its temporary working copies.
The server exposes storage snapshots as `scion_storage_access` and `scion_storage_bytes`
on its existing admin metrics listener; scraping does not query the storage backends.

With a local server whose `main` matches the Git mirror's `HEAD`, and an optional daemon:

```sh
python3 scripts/benchmark-git.py --help
python3 scripts/benchmark-git.py --self-test
python3 scripts/benchmark-git.py \
  --repo /path/to/repo.git --scion target/release/scion \
  --importer target/release/scion-import --endpoint https://127.0.0.1:18443 \
  --ca-file /path/to/ca.pem --token-file /path/to/raw-scion-token \
  --work-dir /tmp/scion-benchmark --output /tmp/scion-results.json \
  --daemon-endpoint unix:///path/to/daemon.sock --server-pid SERVER_PID
```

The work directory must not exist. Omit `--daemon-endpoint` when testing only direct access;
`--importer` adds local full-history Git clones and no-change synchronization checks.

### Concurrent CI workloads

[`scripts/benchmark-scaling.py`](scripts/benchmark-scaling.py) measures shared-cache job
throughput, p50/p95 job latency, and one-file commit publication as concurrency increases.
It runs each implementation's waves separately in randomized order, with two explicit
warmups and three measured waves by default. Checkout bootstrap is inside the timer;
fixture creation, service startup, cache priming, and correctness checks are separate.

Git baselines include independent shared clones borrowing objects from a ready mirror
containing only the default branch, with a reachable commit graph prepared outside the timers,
cone-mode sparse-index clients, fresh shallow filtered clones, and fresh filtered clones
with default-branch history. Cone mode also includes files in ancestor directories; the
runner validates and reports those additional files. Query jobs read a file and the first
100 first-parent repository history entries using the ready Git mirror directly, without charging a
clone. Commit workloads change one file from a pinned parent, with distinct fresh content
per job and run:
Git commits and pushes a private ref into a disposable receiver, while Scion creates a
server-visible changeset without updating `main`. Git keeps its default durability;
Scion keeps production authorization, integrity verification, and durability.

```sh
python3 scripts/benchmark-scaling.py --self-test
python3 scripts/benchmark-scaling.py \
  --repo /path/to/repo.git --scion target/release/scion \
  --endpoint https://127.0.0.1:18444 --metrics-url http://127.0.0.1:19465/metrics \
  --daemon-endpoint unix:///path/to/daemon.sock \
  --ca-file /path/to/ca.pem --token-file /path/to/raw-scion-token \
  --server-pid SERVER_PID --daemon-pid DAEMON_PID \
  --containers COCKROACH_CONTAINER,MINIO_CONTAINER \
  --concurrency 1,4,8 --scopes apps/web/src/lib,apps/web \
  --work-dir /tmp/scion-scaling --output /tmp/scion-scaling.json
```

The output retains every wave and job sample, per-job client CPU/peak RSS on macOS,
server/daemon CPU deltas and sampled RSS, authoritative storage/byte counters, received Git
pack sizes, and materialized bytes. `--containers` adds dedicated backend CPU and memory
from Docker cgroup v2 counters; without it, CPU accounting excludes the database and object
store. Container memory includes page cache and is not directly comparable to host-process
RSS; CPU snapshots include background work and observer overhead. Measured waves refuse
to run alongside Cargo builds or workspace tests.
Temporary Git repositories and job directories are removed; resource reports remain, and
Scion's unreferenced benchmark changesets remain in the configured server. Use a disposable
benchmark server. This is single-machine concurrency testing, not a demonstration of
horizontal scaling, virtualized checkout, enormous-repository behavior, or full-fleet cost.

On the same pinned `t3code` snapshot, the final matrix passed 897 measured jobs across
concurrency 1, 4, and 8 and working sets of 107 and 1,521 files. A separate seven-wave
confirmation passed another 504 jobs. The confirmation below uses the 107-file scope,
a warmed shared daemon, and the fastest measured Git baseline for each row. Both final
runs use a main-only Git receiver with a prepared commit graph. Times are median seconds
per wave, including all jobs in that wave:

| Workload | Concurrent jobs | Git | Scion | Speedup |
| --- | ---: | ---: | ---: | ---: |
| Fresh sparse checkout | 1 | 0.102 | 0.044 | 2.34× |
| Fresh sparse checkout | 8 | 0.333 | 0.216 | 1.54× |
| One-file commit, published | 1 | 0.424 | 0.107 | 3.96× |
| One-file commit, published | 8 | 1.059 | 0.220 | 4.82× |

Every Scion wave in these four confirmation cases was faster than every corresponding Git
wave. The eight-job checkout used 1.00 accounted CPU-seconds versus Git's 2.24; the eight-job
write wave used 0.54 versus 8.33, including Scion's server, daemon, CockroachDB, and MinIO.
Client CPU values are rounded to 0.01 seconds by the macOS observer, so reported zeroes
for short commands do not mean zero work. Warm daemon checkout waves made zero origin
object-storage reads. Commit provisioning is outside the commit timer on both sides;
the timed operation includes scanning, committing, and making the change server-visible.

Git still wins elsewhere: for eight 1,521-file checkouts, the final matrix measured 1.01
seconds for shared-mirror Git versus 2.43 for Scion's daemon. An eight-job file/history
query wave was 0.032 seconds versus 0.112. These results establish advantages for particular
local job workloads, not a general replacement for Git or a reproduction of fleet-scale claims.

The hill climb retained a checkout change that obtains the post-rename stat through the
already-open descriptor, avoiding another parent-directory walk. Its isolated three-wave
experiment reduced large-checkout time by 12–14%, with lower CPU at all three concurrency
levels; the complete rerun showed a smaller gain at eight jobs. A second experiment that
forwarded compressed frames from the node daemon was rejected: it did not improve on the
retained variant over the local socket. Authorization, hashing, atomic replacement,
journaling, and racy timestamp checks remain enabled.

[Compact results and timing/CPU samples](benchmarks/t3code-scaling.json) include every final
and confirmation case plus the selected hill-climb samples. Exploratory before/iteration
runs inherited all Git refs and had no prepared commit graph; they assess only the Scion
runtime change. The headline Git comparisons above use the corrected final fixtures.

## How it works

```text
scion CLI ── Unix socket ──▶ scion-daemon ── TLS/gRPC ──▶ scion-server
    └─────────────────────── TLS/gRPC ──────────────────▶     │
                                                            ├── CockroachDB
                                                            └── S3-compatible storage
```

- **Server-side writes.** Create commits, rebase, merge, and land through APIs. Commits are
  immediately stored on the server; there is no separate push step.
- **Incremental history.** Changesets record file deltas against their first parent, alongside
  a root-tree pointer. Objects use BLAKE3 identifiers and zstd compression.
- **Sparse checkouts.** Fetch missing trees and blobs by identifier, pruning directories
  outside the requested scope. A bounded per-node daemon cache shares objects across jobs.
- **History without a clone.** Query diff, log, blame, ancestry, and merge bases over gRPC.
- **Landing without a global queue.** Rebase a linear stack onto a ref, then update it with
  a versioned compare-and-swap. Concurrent attempts retry with backoff.
- **Derived indexes.** Eager and lazy derived data support path history, blame, commit-graph
  queries, and Git mappings.
- **Access control and redaction.** GitHub device-flow login, service principals, path/ref
  policies, and quarantine/purge of blob or metadata contents without rewriting identifiers.
- **Git migration.** A one-way importer preserves history and records Git-to-Scion mappings.

## Scope

The current v1 targets one monorepo and Unix clients (Linux and macOS). It implements the core
source-control service, CLI, daemon, importer, and test infrastructure. Production throughput
and resilience at Anthropic's scale have not been established.

Several features from the blog are deferred: regional cache servers, daemon snapshot bootstrap,
virtualized checkouts, filesystem-watcher status, and copy detection. `scion-cache` is a library
used by the daemon and client, not a regional cache server. Working copies materialize the files
in their sparse scope, and status uses a stat-cache-based filesystem walk.

The storage stack uses CockroachDB and S3-compatible storage rather than Spanner. Code review,
code search, and CI orchestration are separate systems outside this repository. Offline
operation, Git protocol compatibility, and two-way Git sync are out of scope.

See [the specification](SPEC.md) for contracts and
[deferred work and known edge cases](SPEC.md#appendix-a-deferred-work-and-known-edge-cases)
for current limits.

## Build

Install Rust through rustup and ensure the Protocol Buffers compiler (`protoc`) is on your PATH.
The repository pins Rust 1.98.0 in [`rust-toolchain.toml`](rust-toolchain.toml).

```sh
cargo build --workspace --all-targets
./target/debug/scion --help
```

The workspace builds four executables: `scion`, `scion-server`, `scion-daemon`, and `scion-import`.

## Use

Running the service requires CockroachDB, an S3-compatible bucket, TLS certificates, an
Ed25519 signing key, and an access policy. Configure the server using
[SPEC section 6](SPEC.md#6-configuration-specification), then start it:

```sh
./target/debug/scion-server --config /path/to/scion-server.toml
```

With a configured server and an authorized GitHub account, a typical workflow is:

```sh
export SCION_ENDPOINT=https://scion.example.internal:7443
# For a private CA, also set SCION_CA_FILE to its PEM file.

./target/debug/scion login
./target/debug/scion checkout main /tmp/scion-work --sparse crates/scion-core
./target/debug/scion -C /tmp/scion-work status

# Edit files in the working copy, then:
./target/debug/scion -C /tmp/scion-work diff
./target/debug/scion -C /tmp/scion-work commit -m "Update core"
./target/debug/scion -C /tmp/scion-work land --ref main
```

Use `login --service NAME` with `SCION_SERVICE_SECRET` for a configured service principal.
To use a node daemon, start `scion-daemon --config /path/to/scion-daemon.toml` and point
`SCION_ENDPOINT` at its `unix:///path/to/daemon.sock` endpoint.

Commands support `--json` with versioned output schemas in [`schemas/v1`](schemas/v1).
Run each command with `--help` for options.

## Test

The default suite uses in-memory storage, temporary working copies, and loopback services.
It requires no running database or Docker. Tests include an independent reference model,
Git differentials, fault injection, concurrency checks, and storage-access complexity gates.

```sh
cargo test --workspace
scripts/check.sh                 # Format, build, test, Clippy, and dependency-layer checks
scripts/check.sh --full          # Also run the larger, ignored test cases
```

To exercise CockroachDB and MinIO, install Docker with Compose and run:

```sh
scripts/test-external.sh --up --all
docker compose -f docker/compose.test.yml down -v
```

The final command removes the disposable test containers and their volumes. External-backend
tests skip when their environment variables are unset; some Git differential tests skip when
the Git CLI is unavailable. See [`docs/testing.md`](docs/testing.md) for details.

## Project map

| Area | Crates |
| --- | --- |
| Domain and object formats | `scion-core`, `scion-object` |
| Storage | `scion-store`, `scion-store-crdb`, `scion-store-s3` |
| History and merge algorithms | `scion-history`, `scion-merge`, `scion-derived` |
| Protocol and server | `scion-proto`, `scion-server` |
| Client and caching | `scion-cli`, `scion-wc`, `scion-daemon`, `scion-cache` |
| Security and operations | `scion-auth`, `scion-policy`, `scion-redaction`, `scion-config`, `scion-obs` |
| Migration and verification | `scion-import`, `scion-refmodel`, `scion-testkit`, `scion-conformance` |

For design details, start with [SPEC.md](SPEC.md),
[implementation-defined behavior](docs/implementation-defined.md), and
[storage semantics](docs/contracts/storage-semantics.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
Third-party Git fixtures retain their own license notices in [`corpus/git`](corpus/git).
