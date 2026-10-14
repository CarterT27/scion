# `git merge-file` differential: known differences

The RECOMMENDED SPEC 19.4 check (`tests/merge_git_differential.rs`) compares `merge3` with
`git merge-file -p ours base theirs` on inputs that `line-diff3-v1` merges cleanly. The test is
skipped when `git` is not installed. Observed with git 2.56.0.

## Corpora

| Corpus | Requirement | Observed |
|---|---|---|
| Contract worked examples (`docs/contracts/line-diff3-v1.md` 10.3), clean ones | must agree | all agree |
| 300 seeded cases: unique lines, one edit per side, at least 3 unchanged lines between | must agree | all agree |
| 300 seeded random small-alphabet merges (`a`/`b`/`c`/`d` lines, many repeats) | report only | about 93% of the clean ones agree |

## Classes of disagreement

1. **Diff tie-breaking on repeated lines.** git's xdiff is a Myers variant with its own
   heuristics (split-point search, discarding of frequent lines, and sliding change groups).
   `line-diff3-v1` is the plain forward greedy algorithm with a fixed tie rule, which is
   normative for Scion (SPEC 8.3). When the base has repeated lines the two tools can pick
   different but equally short edit scripts, so the hunks land at different base positions.
   Example: base `a\na\n`, ours `a\n`, theirs `d\na\n`. Scion deletes the LAST `a` (contract
   example D6), so ours' hunk `[1, 2)` does not touch theirs' insertion at `[0, 0)` and the merge
   is clean (`d\na\n`). git deletes the FIRST `a`, the hunks touch, and git reports a conflict.
2. **Different clean bytes.** For the same reason, both tools sometimes merge cleanly but place
   a repeated line on a different side of an unchanged one. Example from the random corpus:
   base `c b b c a c`, ours `c b c b c c`, theirs `b d c a b b b c a c`. Scion gives
   `b d c a b c b b c c` and git gives `b d c a b b c b c c`. Both are valid three-way merges of
   their own diffs.
3. **Adjacency.** Not observed as a disagreement on clean merges: both treat edits with zero
   unchanged lines between them as a conflict, and both conflict on different insertions at the
   same point.

No disagreement has been seen where both diffs are unambiguous (unique lines). Scion's output is
defined by `docs/contracts/line-diff3-v1.md` and the frozen vectors in `testdata/golden/diff/`;
git is a sanity check only.
