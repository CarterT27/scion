#!/usr/bin/env bash
# Enforces the PLAN 1.3 layer allow-table (scripts/layers.toml) over `cargo metadata`.
#
#   scripts/check-layers.sh              check the workspace; exit non-zero on any offending edge
#   scripts/check-layers.sh --self-test  prove that injected forbidden edges are rejected
#
# Requires python3 >= 3.11 (tomllib).
set -euo pipefail
cd "$(dirname "$0")/.."

MODE="check"
case "${1:-}" in
  "") ;;
  --self-test) MODE="self-test" ;;
  -h|--help) sed -n '2,7p' "$0"; exit 0 ;;
  *) echo "usage: $0 [--self-test]" >&2; exit 2 ;;
esac

METADATA_FILE="$(mktemp "${TMPDIR:-/tmp}/scion-metadata.XXXXXX")"
trap 'rm -f "$METADATA_FILE"' EXIT
cargo metadata --format-version 1 --no-deps --offline >"$METADATA_FILE" 2>/dev/null \
  || cargo metadata --format-version 1 --no-deps >"$METADATA_FILE"

python3 - "$MODE" scripts/layers.toml "$METADATA_FILE" <<'PY'
import copy
import json
import sys

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover
    sys.exit("check-layers: python3 >= 3.11 is required (tomllib)")

mode, layers_path, metadata_path = sys.argv[1], sys.argv[2], sys.argv[3]
with open(metadata_path) as f:
    meta = json.load(f)
with open(layers_path, "rb") as f:
    layers = tomllib.load(f)


def edges(meta):
    """(from, to, kind) for every dependency between workspace crates; kind in normal/build/dev."""
    names = {p["name"] for p in meta["packages"]}
    out = []
    for p in meta["packages"]:
        for d in p["dependencies"]:
            if d["name"] in names and d["name"] != p["name"]:
                out.append((p["name"], d["name"], d.get("kind") or "normal"))
    return names, out


def check(meta, layers):
    names, es = edges(meta)
    allow = layers.get("allow", {})
    errors = []
    for n in sorted(names):
        if n not in allow:
            errors.append(f"crate {n} is not listed in [allow] of scripts/layers.toml")
    for n in sorted(allow):
        if n not in names:
            errors.append(f"[allow] lists unknown crate {n}")
    # Allow-table: normal and build edges.
    for frm, to, kind in es:
        if kind == "dev":
            continue
        allowed = allow.get(frm, [])
        if "*" in allowed:
            continue
        if to not in allowed:
            errors.append(f"forbidden {kind} dependency: {frm} -> {to} (not in the PLAN 1.2 allow list of {frm})")
    # Hard check 1: isolated crates.
    for iso in layers.get("isolated", {}).get("crates", []):
        for frm, to, kind in es:
            if frm == iso:
                errors.append(f"isolated crate has a scion dependency: {frm} -> {to} ({kind})")
    # Hard check 2: forbidden upward edges.
    for rule in layers.get("forbidden", []):
        for frm, to, kind in es:
            if kind != "dev" and frm in rule["from"] and to in rule["to"]:
                errors.append(f"forbidden upward dependency: {frm} -> {to} ({kind})")
    # Hard check 3: dev-dependency closure rule.
    for rule in layers.get("no_dev_dependency_on_closure_of", []):
        target = rule["crate"]
        closure, frontier = set(), [target]
        while frontier:
            c = frontier.pop()
            for frm, to, kind in es:
                if frm == c and kind != "dev" and to not in closure:
                    closure.add(to)
                    frontier.append(to)
        for frm, to, kind in es:
            if kind == "dev" and to == target and frm in closure:
                errors.append(f"forbidden dev-dependency: {frm} -> {to} ({frm} is a dependency of {target})")
    return sorted(set(errors))


def inject(meta, frm, to, kind):
    m = copy.deepcopy(meta)
    for p in m["packages"]:
        if p["name"] == frm:
            p["dependencies"].append({"name": to, "kind": None if kind == "normal" else kind,
                                      "optional": False, "source": None, "req": "*"})
            return m
    sys.exit(f"self-test: unknown crate {frm}")


if mode == "check":
    errs = check(meta, layers)
    if errs:
        for e in errs:
            print(f"check-layers: {e}", file=sys.stderr)
        sys.exit(1)
    print(f"check-layers: OK ({len(meta['packages'])} crates)")
    sys.exit(0)

# Self-test: the real workspace must pass, and every injected bad edge must be rejected.
failures = 0
base = check(meta, layers)
if base:
    print("self-test: the unmodified workspace does not pass:", *base, sep="\n  ")
    failures += 1
cases = [
    ("upward edge object -> history", "scion-object", "scion-history", "normal", "forbidden normal dependency"),
    ("upward edge core -> server", "scion-core", "scion-server", "normal", "forbidden upward dependency"),
    ("history -> policy (storage/history must not see policy)", "scion-history", "scion-policy", "normal", "forbidden upward dependency"),
    ("build dependency derived -> obs", "scion-derived", "scion-obs", "build", "forbidden upward dependency"),
    ("refmodel dev-depends on core", "scion-refmodel", "scion-core", "dev", "isolated crate"),
    ("proto dev-depends on testkit", "scion-proto", "scion-testkit", "dev", "forbidden dev-dependency"),
    ("core dev-depends on testkit", "scion-core", "scion-testkit", "dev", "forbidden dev-dependency"),
    ("cache -> history (not in allow list)", "scion-cache", "scion-history", "normal", "forbidden normal dependency"),
]
for title, frm, to, kind, expect in cases:
    errs = check(inject(meta, frm, to, kind), layers)
    hit = [e for e in errs if expect in e and frm in e and to in e]
    status = "rejected" if hit else "NOT REJECTED"
    print(f"self-test: {title}: {status}")
    if not hit:
        failures += 1
# A permitted dev edge must still pass.
errs = check(inject(meta, "scion-history", "scion-testkit", "dev"), layers)
print(f"self-test: permitted dev edge history -> testkit: {'accepted' if not errs else 'WRONGLY REJECTED'}")
if errs:
    failures += 1
if failures:
    print(f"self-test: FAILED ({failures})", file=sys.stderr)
    sys.exit(1)
print("self-test: OK")
PY
