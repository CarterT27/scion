# Contract amendments

The public API in `docs/PLAN.md` section 2, the proto `proto/scion/v1/scion.proto`, every
`Cargo.toml`, `Cargo.lock`, every `src/lib.rs`, and the contract documents in `docs/contracts/`
are frozen after wave 0 (PLAN 2.0, 5.2).

## Protocol

1. A package that needs a breaking or cross-crate change (a changed or removed frozen signature,
   a new dependency, a manifest or `lib.rs` edit, a proto change) does NOT make it. It states an
   amendment request in its final report with:
   - **File** and **item** (path plus Rust item / proto message / manifest key).
   - **Old signature** (exact text).
   - **New signature** (exact text).
   - **Reason** (which SPEC requirement or test needs it, and why an additive change in owned
     files is insufficient).
2. Additive `pub` items inside a package's own files need no amendment.
3. The orchestrator batches the requests of a wave, applies them between waves as one serialized
   patch, regenerates `Cargo.lock` when needed, re-runs `scripts/check.sh` on the result, and logs
   every applied amendment below.
4. Gate G0 (after wave 0) may fix compile, consistency or SPEC-mismatch defects in P00-owned
   files only; every such change is logged below as well.

## Log

| # | Wave | Requested by | File / item | Change | Reason |
|---|---|---|---|---|---|
| G0-1 | 0 (G0) | Gate G0 | `crates/scion-proto/src/raw.rs` / `raw::RawCodec` | Added `impl tonic::codec::Codec for RawCodec` with `Encode = Decode = bytes::Bytes`, `Encoder = RawEncoder`, `Decoder = RawDecoder` (new additive pub unit structs). `encoder()`/`decoder()` are real; `RawEncoder::encode` / `RawDecoder::decode` are `todo!("P08")`. | PLAN 2.13 freezes `RawCodec` as a `tonic::codec::Codec` with `Encode = Decode = Bytes`, but P00 had only the bare struct (missing stub). The associated types are now frozen for P16. |
| G0-2 | 0 (G0) | Gate G0 | `crates/scion-server/src/authz/mod.rs` / `ObjectAuthorizer`; `authz/object.rs` | Private field `state: object::AuthzState` on `ObjectAuthorizer`, built with `Default` in `ObjectAuthorizer::new`; `pub(crate) struct AuthzState {}` (derive `Debug, Default`) declared in `authz/object.rs`, which P25 owns and fills. No public signature changed. | PLAN 2.14 requires `can_read` to cache decisions by `(principal, id, policy_epoch)`, but the struct lives in the P00-owned `authz/mod.rs`, so P25 had nowhere to keep that state. P25 must keep `AuthzState: Send + Sync`. |
| G0-3 | 0 (G0) | Gate G0 | `crates/scion-wc/src/state.rs` / `WcState.tracking_ref` | `#[serde(with = "crate::serde_util::opt_ref")]` became `#[serde(default, with = "crate::serde_util::opt_ref")]`. Regression test `serde_util::tests::wc_state_toml_round_trip_without_optional_fields`. | TOML omits a `None` field, and a `with` field without `default` is required on input, so a saved state with no tracking ref failed to load ("missing field `tracking_ref`"; reproduced before the fix). |
| G0-4 | 0 (G0) | Gate G0 | `docker/compose.test.yml` | MinIO: `minio/minio` + `minio/mc` (+ `minio-setup` one-shot) replaced by `bitnamilegacy/minio:2025.7.23-debian-12-r5` with `MINIO_DEFAULT_BUCKETS=scion-test` and a curl readiness healthcheck. CockroachDB: command `start-single-node --insecure` (dropped `--listen-addr=0.0.0.0:26257 --http-addr=0.0.0.0:8080` and the 8080 port). | The pinned MinIO images do not exist (Docker Hub `minio/minio` repository is gone; quay.io denies anonymous pulls). CockroachDB v24.3.5 exits with "hostname of listen_addr must be 127.0.0.1 or localhost" in insecure mode. Verified: `docker compose up -d --wait` is healthy, SQL works at the `scripts/test-external.sh` URL, and put/get to bucket `scion-test` works with `minioadmin`. |
| G0-5 | 0 (G0) | Gate G0 | `crates/scion-cli/Cargo.toml`, `crates/scion-derived/Cargo.toml`, `crates/scion-conformance/Cargo.toml`, `Cargo.lock` | scion-cli: + `bytes`, `futures`, `tokio-stream`, `zstd`. scion-derived: + `hex`. scion-conformance: + `zstd`. No new packages in `Cargo.lock`. | scion-cli receives `GetObjects` zstd frames and must decompress before verifying (SPEC 7.3), and consumes streaming RPCs (PLAN 5.3 lists bytes/futures/tokio-stream for all async crates). scion-derived formats `g:<40hex>` git_mapping keys (PLAN 6.4). Conformance digest-checks served frames (16.2). Without these, wave 5/6 would need manifest amendments. |
| G0-6 | 0 (G0) | Gate G0 | Accepted P00 deviations (no change) | (a) `scion_cache::LimiterPermit` and `scion_wc::lock::WcLock` have a private field instead of being unit structs. (b) `GotoReport`, `RebaseReport`, `LandReport`, `FileChangeView` got String-based fields chosen by P00; the owning packages (P18, P28) may change them because PLAN 2.17 freezes only the names and derives. (c) `StatusReport`/`WcState` use `#[serde(with = ...)]` helpers in `crates/scion-wc/src/serde_util.rs`; that file is in no `owns` list and stays P00-owned. (d) `Credentials` has a hand-written redacting `Debug` instead of a derived one. (e) Names PLAN left open: `scion_proto::Transport = InterceptedService<EpochCapture<Channel>, BearerInterceptor>`, `scion_server::grpc::GrpcServices`, `handlers::BoxStream<T>`. (f) `FaultRegistry::check` increments a per-point hit counter on every call, because `hits()` must count unarmed checks; the armed/crashed decision is still one relaxed atomic load. (g) Workspace clippy `type_complexity = "allow"`. | (a) A unit struct cannot hold a semaphore permit or a file lock. (b) `ObjectId`/`RepoPath` do not implement `JsonSchema`. (c) scion-object has no serde dependency. (d) The derived impl would leak tokens. (f) PLAN 2.1 asks for both properties. (g) Several frozen signatures trip that lint. |
| G0-7 | 0 (G0) | Gate G0 | OPEN, for the orchestrator (no change made) | (1) P23 `testing::InProcessServer` "TLS from TestTls" conflicts with PLAN 1.2/1.3: scion-server may not depend on scion-testkit. Recommended: no change; P23 generates TLS with `rcgen`, which is already a normal dependency of scion-server. (2) PLAN 2.13 `convert` mentions scion-store row types, but scion-proto may depend only on core and object. scion-store cannot be added, because scion-store dev-depends on scion-testkit and scion-testkit depends on scion-proto (PLAN 1.2 dev rule, checked by `check-layers.sh`). Recommended: the `RefRow`/`RefLogRow`/`LandedEntry` <-> proto conversions live in scion-server (P23/P26 files), and P08 covers core and object types only. | Both are plan-text conflicts that G0 may not resolve by editing PLAN. |
