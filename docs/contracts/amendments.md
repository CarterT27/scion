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
| | | | | (none yet) | |
