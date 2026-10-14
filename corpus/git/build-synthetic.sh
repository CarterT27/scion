#!/usr/bin/env bash
# Builds corpus/git/synthetic-edge.bundle with the git CLI (P09; SPEC 19.0 Corpus, 19.14).
#
# The history is fully deterministic (fixed identities, dates and zones; plumbing only), so
# re-running this script reproduces the committed bundle's commit ids. Tests never run this
# script: they read the committed bundle with gix (no git CLI required).
#
# Shapes covered (see README.md for the commit-by-commit table):
#   - multiple roots (three parentless commits, one with an empty tree)
#   - empty commits (same tree as the parent)
#   - an octopus merge (four parents)
#   - a merge keeping the first parent's version of a conflicting file
#   - mode 100664 (legacy group-writable blob mode; written with `git mktree`)
#   - executable files and an executable flip, symlinks and a symlink retarget
#   - a gitlink (mode 160000) at vendor/sub
#   - rename-free line edits of one file (blame.txt) for blame differentials
#
# Usage: corpus/git/build-synthetic.sh [output.bundle]
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
out="${1:-$here/synthetic-edge.bundle}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

export HOME="$work"
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME="Scion Fixture Author"
export GIT_AUTHOR_EMAIL="author@example.com"
export GIT_COMMITTER_NAME="Scion Fixture Committer"
export GIT_COMMITTER_EMAIL="committer@example.com"

git init -q -b main "$work/repo"
cd "$work/repo"
git config core.autocrlf false
git config core.fileMode false

t=1700000000
tick() {
  t=$((t + 3600))
  export GIT_AUTHOR_DATE="@$t +0130"
  export GIT_COMMITTER_DATE="@$((t + 7)) -0700"
}

# put <git mode> <path> <content>: stage a blob (or symlink target) with an explicit mode.
put() {
  local sha
  sha="$(printf '%s' "$3" | git hash-object -w --stdin)"
  git update-index --add --cacheinfo "$1,$sha,$2"
}

# gitlink <path> <40-hex sha>: stage a submodule entry.
gitlink() {
  git update-index --add --cacheinfo "160000,$2,$1"
}

rm_path() {
  git update-index --force-remove "$1"
}

# tree: write the index as a tree; group.txt at the root gets mode 100664 (git normalizes it
# to 100644 in the index, so the root tree is rewritten with mktree).
tree() {
  local raw
  raw="$(git write-tree)"
  git ls-tree "$raw" | sed -E $'s/^100644 (blob [0-9a-f]+\tgroup\\.txt)$/100664 \\1/' | git mktree
}

# commit <branch> <message> [parents...]: commit the current index tree to <branch>; sets $C.
# (Not called in a subshell, so the clock in tick() advances.)
commit() {
  local branch="$1" msg="$2" c
  shift 2
  local args=()
  for p in "$@"; do args+=(-p "$p"); done
  tick
  c="$(printf '%s\n' "$msg" | git commit-tree "$(tree)" ${args[@]+"${args[@]}"})"
  git update-ref "refs/heads/$branch" "$c"
  C="$c"
}

# commit_tree <branch> <message> <tree> [parents...]: commit an explicit tree; sets $C.
commit_tree() {
  local branch="$1" msg="$2" tr="$3" c
  shift 3
  local args=()
  for p in "$@"; do args+=(-p "$p"); done
  tick
  c="$(printf '%s\n' "$msg" | git commit-tree "$tr" ${args[@]+"${args[@]}"})"
  git update-ref "refs/heads/$branch" "$c"
  C="$c"
}

load() { git read-tree "$1"; }
empty_index() { git read-tree --empty; }

lines() {
  # lines <prefix> <n>: n numbered lines.
  local i
  for i in $(seq 1 "$2"); do printf '%s %s\n' "$1" "$i"; done
}

# 1. root commit on main.
empty_index
put 100644 README "$(printf 'Synthetic edge repository\nline two\nline three\n')"$'\n'
put 100644 blame.txt "$(lines blame 10)"$'\n'
put 100755 tools/run.sh "$(printf '#!/bin/sh\necho run\n')"$'\n'
put 120000 link "README"
put 100644 group.txt "group writable"$'\n'
commit main "root: initial files"; c1="$C"

# 2. rename-free line edits of blame.txt (modify line 3, insert after line 7).
load "$c1"
put 100644 blame.txt "$(lines blame 2; printf 'blame 3 edited\n'; for i in 4 5 6 7; do printf 'blame %s\n' "$i"; done; printf 'blame 7.5 inserted\n'; lines blame 10 | tail -n 3)"$'\n'
commit main "blame: edit line 3, insert after line 7" "$c1"; c2="$C"

# 3. empty commit (same tree as the parent).
commit_tree main "empty commit" "$(git rev-parse "$c2^{tree}")" "$c2"; c3="$C"

# 4. three branches off c3 with disjoint changes, then an octopus merge.
load "$c3"
put 100644 blame.txt "$(lines blame 2; printf 'blame 3 edited\n'; for i in 4 5 6 7; do printf 'blame %s\n' "$i"; done; printf 'blame 7.5 inserted\n'; printf 'blame 8\nblame 9 on f1\nblame 10\n')"$'\n'
commit f1 "f1: edit blame line 9" "$c3"; f1="$C"
load "$c3"
put 100644 f2.txt "added on f2"$'\n'
commit f2 "f2: add f2.txt" "$c3"; f2="$C"
load "$c3"
put 100644 dir/f3.txt "added on f3"$'\n'
commit f3 "f3: add dir/f3.txt" "$c3"; f3="$C"
load "$f1"
put 100644 f2.txt "added on f2"$'\n'
put 100644 dir/f3.txt "added on f3"$'\n'
commit main "octopus: merge f1, f2, f3" "$c3" "$f1" "$f2" "$f3"; c4="$C"

# 5. more line edits, a symlink retarget and an executable flip.
load "$c4"
put 100644 blame.txt "$(printf 'blame 0 prepended\n'; lines blame 2; printf 'blame 3 edited\n'; for i in 4 5 6 7; do printf 'blame %s\n' "$i"; done; printf 'blame 7.5 inserted\n'; printf 'blame 8\nblame 9 on f1\n')"$'\n'
put 120000 link "blame.txt"
put 100644 tools/run.sh "$(printf '#!/bin/sh\necho run\n')"$'\n'
commit main "blame: prepend line 0, drop line 10; retarget link; run.sh not executable" "$c4"; c5="$C"

# 6. a gitlink.
load "$c5"
gitlink vendor/sub 0123456789abcdef0123456789abcdef01234567
commit main "add gitlink vendor/sub" "$c5"; c6="$C"

# 7. a conflicting side branch merged keeping the first parent's version of README.
load "$c6"
put 100644 README "$(printf 'Synthetic edge repository\nline two from side\nline three\n')"$'\n'
put 100644 side.txt "side file"$'\n'
commit side "side: edit README line 2, add side.txt" "$c6"; s1="$C"
load "$c6"
put 100644 README "$(printf 'Synthetic edge repository\nline two from main\nline three\n')"$'\n'
put 100755 tools/run.sh "$(printf '#!/bin/sh\necho run\n')"$'\n'
commit main "main: edit README line 2; run.sh executable again" "$c6"; c7="$C"
load "$c7"
put 100644 side.txt "side file"$'\n'
commit main "merge side keeping the first parent's README" "$c7" "$s1"; c8="$C"

# 8. two more roots: an orphan with content and an orphan with an empty tree, both merged.
empty_index
put 100644 other/root.txt "second root"$'\n'
commit orphan "orphan root: other/root.txt"; o1="$C"
commit_tree empty-root "orphan root with an empty tree" "$(git hash-object -t tree -w /dev/null)"; e1="$C"
load "$c8"
put 100644 other/root.txt "second root"$'\n'
commit main "merge unrelated histories: orphan and empty-root" "$c8" "$o1" "$e1"; c9="$C"

# 9. a trailing empty commit and a final blame edit.
commit_tree main "empty commit 2" "$(git rev-parse "$c9^{tree}")" "$c9"; c10="$C"
load "$c10"
put 100644 blame.txt "$(printf 'blame 0 prepended\n'; lines blame 2; printf 'blame 3 edited twice\n'; for i in 4 5 6 7; do printf 'blame %s\n' "$i"; done; printf 'blame 7.5 inserted\n'; printf 'blame 8\nblame 9 on f1\n')"$'\n'
commit main "blame: edit line 3 again" "$c10"

git symbolic-ref HEAD refs/heads/main
git bundle create "$out" --all 2>/dev/null
echo "wrote $out"
git log --all --format='%H %P %s' --topo-order
