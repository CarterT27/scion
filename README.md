# Scion

Source control for always-online clients, coding agents, and CI.

Scion is an independent, clean-room Rust implementation inspired by Anthropic's
[Redesigning source control](https://www.anthropic.com/engineering/redesigning-source-control).
It explores the architecture described in that post: keep history and expensive operations
on the server, fetch immutable objects as needed, and check out only the paths a job needs.
This repository is not Anthropic's implementation and is not affiliated with Anthropic.

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
