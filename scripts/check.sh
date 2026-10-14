#!/usr/bin/env bash
# The definition of done of every package (docs/PLAN.md section 0):
#   cargo fmt --all -- --check
#   cargo build --workspace --all-targets
#   cargo test --workspace
#   cargo clippy --workspace --all-targets -- -D warnings
#   scripts/check-layers.sh
#
#   scripts/check.sh          run the five checks
#   scripts/check.sh --full   then run the spec-sized `#[ignore] full_*` tests:
#                             SCION_FULL=1 cargo test --workspace --release -- --include-ignored
set -euo pipefail
cd "$(dirname "$0")/.."

FULL=0
for arg in "$@"; do
  case "$arg" in
    --full) FULL=1 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "usage: $0 [--full]" >&2; exit 2 ;;
  esac
done

step() {
  echo "==> $*"
  "$@"
}

step cargo fmt --all -- --check
step cargo build --workspace --all-targets
step cargo test --workspace
step cargo clippy --workspace --all-targets -- -D warnings
step scripts/check-layers.sh

if [ "$FULL" = 1 ]; then
  echo "==> SCION_FULL=1 cargo test --workspace --release -- --include-ignored"
  SCION_FULL=1 cargo test --workspace --release -- --include-ignored
fi

echo "check.sh: all checks passed"
