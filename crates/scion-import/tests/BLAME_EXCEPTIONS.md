# Blame exceptions (SPEC 19.6)

`tests/import_blame.rs` compares the Blame RPC on imported corpora with
`git blame --porcelain` at the imported tip (`HEAD` of each vendored bundle), line by line:
origin commit (mapped through `git_mapping`) and origin line number.

- Files whose history `git blame` follows through a rename are out of scope (SPEC 19.6 "files
  without renames") and are skipped automatically (cfg-if: `.github/workflows/main.yaml`).
- Every other file must agree, except the files listed below. For a listed file, at most
  `Max lines` lines may differ, and the test still checks every differing line on the Scion
  side: the file at Scion's origin changeset must hold the identical line at Scion's origin
  line number. So a listed difference is always a different but valid choice among equal
  lines, never a wrong attribution.

## Why the two tools disagree

Scion's blame (SPEC 9.2) diffs with the normative line-diff3-v1 Myers algorithm
(`docs/contracts/line-diff3-v1.md`): plain forward greedy Myers with fixed tie-breaking and no
heuristics. `git blame` runs xdiff, which differs exactly where a line occurs more than once
on both sides:

- xdiff discards lines that have no match or very many matches before running Myers
  (`xdl_cleanup_records`), then re-inserts them, and shifts change groups to "nicer" positions
  (`xdl_change_compact`, including the indent heuristic).
- When a run of equal lines (blank lines, `}`, `    }`, `(`, `unsafe {`) could be matched at
  several positions, the two alignments pick different occurrences. The line content is
  identical, but the occurrence picked can come from a different commit or line number.
- At merges, git blame passes the whole file to a parent whose blob is identical (any parent,
  in order), while Scion's history node is reused only from the first parent (SPEC 9.2) and
  otherwise attributes lines parent by parent. Both give the same content, but equal lines
  may again resolve to different occurrences.

All differing lines observed with git 2.5x are blank or punctuation-only lines, plus one
`unsafe {` line in itoa `src/lib.rs`.

| Corpus | Path | Max lines | Reason |
|---|---|---|---|
| `cfg-if` | `CHANGELOG.md` | 3 | blank lines between release sections align differently |
| `cfg-if` | `src/lib.rs` | 1 | a lone `    (` line matched at another occurrence |
| `itoa` | `README.md` | 9 | blank lines and a `}` line inside code blocks |
| `itoa` | `benches/bench.rs` | 1 | a blank line |
| `itoa` | `fuzz/Cargo.toml` | 1 | a blank line (same commit, other occurrence) |
| `itoa` | `src/lib.rs` | 32 | blank lines and closing braces across many refactors; one `unsafe {` line |
| `itoa` | `tests/test.rs` | 2 | blank lines |
