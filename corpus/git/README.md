# Vendored git histories (SPEC 19.0 Corpus, 19.14)

This directory holds the git histories used by the import differential tests (P29) and the
corpus tests (P09). Each history is a `git bundle` holding the full history (`git bundle create
<file> --all`). Tests read the bundles with `gix` through `scion_testkit::git::{bundles, unpack}`,
so the `git` CLI is not needed at test time.

## Real open-source histories

| Bundle | Source URL | Bundled tip | Commits / merges | License | License file |
|---|---|---|---|---|---|
| `cfg-if.bundle` | https://github.com/rust-lang/cfg-if | `refs/heads/main` = `14432a28e180ec717933714aed7828fc7ed2fd29` | 137 / 25 | MIT OR Apache-2.0 | `LICENSE-cfg-if` |
| `itoa.bundle` | https://github.com/dtolnay/itoa | `refs/heads/master` = `1577ed901354d0d7448ac162328f9dbf5183124c` | 330 / 31 | MIT OR Apache-2.0 | `LICENSE-itoa` |

- Both were cloned with `git clone --bare <url>` on 2026-10-14 and bundled with
  `git bundle create <name>.bundle --all`. They include every branch and tag that existed at
  that time.
- Licenses were checked in each repository's `Cargo.toml` (`license = "MIT OR Apache-2.0"`).
  Both license texts (`LICENSE-MIT` and `LICENSE-APACHE` at the bundled commit) are copied
  verbatim into `LICENSE-<repo>`, under a header that records the source URL, commit and license.
- Size limit: each bundle is at most 5 MB (cfg-if 857,526 bytes; itoa 1,163,910 bytes).
  `crates/scion-testkit/tests/corpus_git.rs` enforces the limit.

## Synthetic edge history

`synthetic-edge.bundle` is built by `build-synthetic.sh` with git plumbing. Identities, dates
and time zones are fixed, so the script reproduces the committed bundle byte for byte (checked
with git 2.56). Tests never run the script. Regenerate it with:

```
corpus/git/build-synthetic.sh            # writes corpus/git/synthetic-edge.bundle
```

Branches: `main` (HEAD), `f1`, `f2`, `f3`, `side`, `orphan`, `empty-root`.

| # | Commit (subject) | Parents | Edge case |
|---|---|---|---|
| 1 | root: initial files | none | root commit; `group.txt` with mode 100664; `tools/run.sh` 100755; `link` symlink (120000) to `README` |
| 2 | blame: edit line 3, insert after line 7 | 1 | rename-free line edits of `blame.txt` |
| 3 | empty commit | 2 | empty commit (tree equals the parent's) |
| 4 | f1 / f2 / f3 | 3 each | three branches with disjoint changes |
| 5 | octopus: merge f1, f2, f3 | 3, f1, f2, f3 | octopus merge (4 parents) |
| 6 | blame: prepend line 0, drop line 10; retarget link; run.sh not executable | 5 | line edits; symlink retarget; executable flip (100755 to 100644) |
| 7 | add gitlink vendor/sub | 6 | gitlink (160000, sha `0123456789abcdef0123456789abcdef01234567`) |
| 8 | side: edit README line 2, add side.txt | 7 | conflicting edit on a side branch |
| 9 | main: edit README line 2; run.sh executable again | 7 | conflicting edit on main; executable flip back |
| 10 | merge side keeping the first parent's README | 9, 8 | merge keeping the first parent's version of a conflicting file (plus the side's new file) |
| 11 | orphan root: other/root.txt | none | second root |
| 12 | orphan root with an empty tree | none | third root, empty tree |
| 13 | merge unrelated histories: orphan and empty-root | 10, 11, 12 | multiple roots joined (octopus of unrelated histories) |
| 14 | empty commit 2 | 13 | empty commit after a merge |
| 15 | blame: edit line 3 again | 14 | rename-free line edit for blame |

Tip of `main`: `b6808d77dd65cb261d7defb18a21698ec53e068a`. The synthetic history was written for
this project and carries no third-party content; it is covered by the workspace license.

The importer (SPEC 14) must fail on the gitlink unless `--skip-submodules` is given, and with
that flag it must record the skipped gitlink (`META_EXT_SKIPPED_GITLINKS`).
