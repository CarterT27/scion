# testdata

Frozen test vectors. Any change to a file here fails CI (SPEC 19.1): each directory is covered by
a test asserting a BLAKE3 digest constant over its sorted file contents.

| Directory | Owner | Contents |
|---|---|---|
| `golden/objects/*.json` | P01 | Canonical encodings and ids for every object kind. One case per file: `{name, kind, description, hex, id_text}`. At least 14 cases (empty tree, all four modes, even extension, Meta with and without committer, Meta with tag 2, root changeset, two-parent changeset with mixed Put/Delete, changeset with an even extension, HistoryNode File and Dir, a blob). |
| `golden/diff/*.json` | P02 | `line-diff3-v1` vectors, at least 60: diff cases `{a_hex, b_hex, edits}` and merge cases `{base_hex, ours_hex, theirs_hex, result_hex}` or `{..., conflicts}`. They include every worked example of `docs/contracts/line-diff3-v1.md` section 10. |

Rules:

- Byte strings are lowercase hex so files are exact and diff-friendly.
- Vectors are produced by the owning package from constructors in code and checked both ways
  (re-encode equals the file; decode of the file equals the constructor). P17 verifies that the
  independent reference model reproduces them.
- If an implementation disagrees with a vector, the implementation is fixed. Changing a vector
  requires an amendment (`docs/contracts/amendments.md`).
