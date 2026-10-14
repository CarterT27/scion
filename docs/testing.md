# Testing

Summary of `docs/PLAN.md` section 3. SPEC 19 lists the required behaviours; Appendix A of the plan
maps each SPEC 19.x item to the package whose tests cover it.

## Hermetic by default

`cargo test --workspace` needs no external service, no network (loopback only) and no Docker:

- `Storage::in_memory` (`MemMetaStore` + `MemBlobStore`) stands in for CockroachDB and S3. It is a
  faithful model: strictly serializable (one mutex over `im` maps), stale snapshots for
  `Consistency::Stale`, forced transaction failures (`fail_txn_every`), and the exact CAS /
  `ON CONFLICT DO NOTHING` / landed-uniqueness semantics of `docs/contracts/storage-semantics.md`.
- `scion_server::testing::InProcessServer` serves on loopback with TLS certificates generated per
  test, so SPEC 17 is exercised even in unit tests.
- `scion_daemon::testing::InProcessDaemon` serves on a temporary Unix socket.
- `scion_testkit::FakeGitHub` (axum on loopback) and `scion_testkit::FakeUpstream` (an in-memory
  `scion.v1` upstream) replace GitHub and the server for client and daemon tests.
- Temporary directories come from `tempfile`.

The `git` CLI is optional: differential tests (`git check-ignore`, `git ls-tree`, `git blame`,
`git merge-file`) and fixture builders print `SKIP: git not found` and pass when it is absent.
Vendored bundles under `corpus/git/` are read with `gix`.

## Fault injection and oracles

- `scion_core::FaultRegistry` arms named points: `scion_core::fault::points` (SPEC 5.3 steps,
  land, redaction), `scion_wc::points` (client operations), and the storage decorator points
  `meta.<method>.before|after` / `blob.<method>.before|after`. Actions: `Error(code)`, `Crash`
  (latches until `reset`), `Delay`, `FailAfter` (effect applied, then UNAVAILABLE).
- Storage access counters (`scion_store::Counters`, `CounterSnapshot`) back the complexity gates.
- Oracles: the independent reference model (`scion-refmodel`), frozen golden vectors
  (`testdata/golden/`), `scion_store::fsck` (SPEC 4.5), the linearizability checker
  (`scion_testkit::lincheck`), the whole-system differential (P32) and the external git
  differentials.

## Heavy counts

Default runs use `scion_core::test_support::proptest_cases(N)` (about 10^4 where the SPEC asks for
10^6, 10^3 where it asks for 10^5). Spec-sized runs are `#[ignore]` tests named `full_*`.

| Variable | Effect |
|---|---|
| `SCION_PROPTEST_CASES` | Overrides every `proptest_cases(default)` |
| `SCION_FULL=1` | Set by `scripts/check.sh --full`; `scion_core::test_support::full_mode()` returns true |

## External backends (gated)

| Variable | Meaning |
|---|---|
| `SCION_TEST_CRDB_URL` | e.g. `postgresql://root@localhost:26257/defaultdb?sslmode=disable`; each test creates `scion_test_<random>` |
| `SCION_TEST_S3_ENDPOINT` | Gate for S3 tests, e.g. `http://localhost:9000` |
| `SCION_TEST_S3_BUCKET` | Default `scion-test` |
| `SCION_TEST_S3_ACCESS_KEY`, `SCION_TEST_S3_SECRET_KEY` | MinIO defaults `minioadmin` / `minioadmin` in `scripts/test-external.sh` |
| `SCION_TEST_S3_REGION` | Default `us-east-1` |

Gated tests are named `external_*`; without their variables they print `SKIP: ...` and pass.
`scion_conformance::Backend::from_env()` adds the External backend to the conformance suites when
both gates are set.

## Commands

| Command | What it runs |
|---|---|
| `scripts/check.sh` | `cargo fmt --all -- --check`, `cargo build --workspace --all-targets`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `scripts/check-layers.sh` (the definition of done of every package) |
| `scripts/check.sh --full` | The above, then `SCION_FULL=1 cargo test --workspace --release -- --include-ignored` (conformance is claimed only on a full run) |
| `scripts/check-layers.sh` | PLAN 1.3 layer allow-table check over `cargo metadata` |
| `scripts/check-layers.sh --self-test` | Proves that injected forbidden edges are rejected |
| `docker compose -f docker/compose.test.yml up -d --wait` | CockroachDB (single node, insecure, 26257) and MinIO (9000) with the `scion-test` bucket |
| `scripts/test-external.sh` | Exports the gate variables and runs `cargo test --workspace -- external_` |
| `scripts/test-external.sh --all` | Exports the gate variables and runs the whole workspace test suite (conformance suites also use the External backend) |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | Documentation must build without warnings |

## Where tests live

- Unit tests: inside the owning module (`#[cfg(test)]`).
- Crate integration tests: `crates/<crate>/tests/<prefix>_*.rs`, with shared helpers in
  `tests/<prefix>_support/mod.rs`; prefixes are listed in each package's `owns`.
- Cross-cutting SPEC 19 suites: `crates/scion-conformance/tests/*.rs`, one file per package.
- Reusable scenarios: `src/testing.rs` behind a `testing` feature (scion-server, scion-daemon,
  scion-wc, scion-import), because integration-test modules cannot be imported across crates.

Dependency rule: scion-core, scion-object, scion-proto and scion-refmodel (the crates
scion-testkit depends on) must not dev-depend on scion-testkit; `scripts/check-layers.sh`
enforces it. scion-refmodel has no scion dependency of any kind.
