# Contract: `line-diff3-v1` (normative)

Status: normative. Owner of this document: P00 (frozen). Implementations: `scion-merge`
(production, P02) and `scion-refmodel::diff` (oracle, P07). SPEC references: 8.3, 9.2, 11.7, 19.4.

The SPEC says the reference implementation's tie-breaking is normative for content-merge output
(SPEC 8.3). This document IS that reference: it defines the algorithm exactly, step by step, so
that two independent implementations produce byte-identical results. If an implementation and
this document disagree, the document wins and the implementation is fixed. If the document is
ambiguous, its worked examples (section 10) and the frozen golden vectors in
`testdata/golden/diff/` decide; file an amendment to clarify the text.

Notation: sequences are 0-indexed; `a..b` is the half-open range of indices `a <= i < b`; `|s|`
is the length of `s`.

## 1. Line splitting

`split_lines(bytes) -> list<line>`:

1. Scan the bytes left to right. Every `0x0A` (`\n`) ends a line; the line INCLUDES its `\n`.
2. If bytes remain after the last `\n` (or there is no `\n` at all and the input is non-empty),
   those bytes form one final, unterminated line.
3. The empty input has zero lines. `"\n"` is one line (`"\n"`). `"a"` is one line (`"a"`).
   `"a\n"` is one line (`"a\n"`); it is NOT two lines.
4. No other byte is special. In particular `\r` is an ordinary byte: `"x\r\n"` and `"x\n"` are
   different lines, and no CRLF normalization ever happens.

Two lines are equal iff their byte strings are equal (bytewise; terminators included). So an
unterminated `"y"` differs from `"y\n"`.

## 2. Myers forward greedy diff (exact)

Input: line sequences `a` (old, length `N`) and `b` (new, length `M`). The algorithm is the
O(ND) forward greedy algorithm of Myers (1986) with one fixed tie-breaking rule and NO
heuristics, NO cost cap, NO divide-and-conquer (linear-space) variant, NO prefix/suffix
stripping that could change the result. Implementations may use any data structures, but MUST
produce exactly the edit script that the following pseudocode produces.

```text
MAX = N + M
V = map from diagonal k to x; initially V[1] = 0          # only V[1] is defined
trace = []                                                # V snapshots, one per round
for d in 0, 1, ..., MAX:
    for k in -d, -d+2, ..., d-2, d:
        if k == -d or (k != d and V[k-1] < V[k+1]):
            x = V[k+1]                 # move DOWN from diagonal k+1: an insertion of b[y-1]
        else:
            x = V[k-1] + 1             # move RIGHT from diagonal k-1: a deletion of a[x-1]
        y = x - k
        while x < N and y < M and a[x] == b[y]:      # follow the snake
            x = x + 1; y = y + 1
        V[k] = x
        if x >= N and y >= M:
            trace.append(copy of V)    # V after (partial) round d
            D = d
            goto BACKTRACK
    trace.append(copy of V)            # V after round d
```

Notes (all normative):

- The selection rule is evaluated exactly as written. In particular, when `V[k-1] == V[k+1]`
  the RIGHT move (a deletion) is chosen; the DOWN move is chosen only when `k == -d` or when
  `V[k-1] < V[k+1]` strictly (and `k != d`).
- `V[k-1]` and `V[k+1]` are only read when they were written in round `d-1` (the selection rule
  guarantees this). Diagonals are not clamped to the grid: `x` may exceed `N` or `y` may exceed
  `M` on diagonals that cannot reach the end; such values are kept as computed.
- The loop stops at the first `(d, k)` for which `x >= N and y >= M`; `D` is that `d` (the edit
  distance in single-line insertions plus deletions).

## 3. Backtracking (exact)

Let `V_d` be `trace[d]` (the snapshot after round `d`) and let `V_{-1}` be the initial map
`{1: 0}`. Starting from `(x, y) = (N, M)`:

```text
ops = []                                   # built in reverse
for d in D, D-1, ..., 0:
    v = V_{d-1}
    k = x - y
    if k == -d or (k != d and v[k-1] < v[k+1]):
        prev_k = k + 1                     # we arrived by a DOWN move (insertion)
    else:
        prev_k = k - 1                     # we arrived by a RIGHT move (deletion)
    prev_x = v[prev_k]
    prev_y = prev_x - prev_k
    while x > prev_x and y > prev_y:       # walk back along the snake
        ops.append(EQUAL(a: x-1, b: y-1)); x = x - 1; y = y - 1
    if d > 0:
        if x == prev_x: ops.append(INSERT(b: prev_y))      # line b[prev_y]
        else:           ops.append(DELETE(a: prev_x))      # line a[prev_x]
    (x, y) = (prev_x, prev_y)
reverse(ops)
```

The same selection rule as in section 2 is used. The result is a sequence of single-line
operations that transforms `a` into `b`: EQUAL ops consume one line of each (and the lines are
equal), DELETE consumes one line of `a`, INSERT consumes one line of `b`.

## 4. Edit-script normalization and the `Edit` runs

1. A *changed run* is a maximal contiguous sequence of non-EQUAL ops. Within every changed run,
   reorder the ops so that all DELETEs come first (in their original order) followed by all
   INSERTs (in their original order). The deleted lines of a run are contiguous in `a` and the
   inserted lines are contiguous in `b`, so this is well defined.
2. Coalesce consecutive ops of the same type into runs:
   - `Equal { a, b, len }`: `len >= 1` equal lines starting at `a[a]` and `b[b]`.
   - `Delete { a, len }`: `len >= 1` lines of `a` starting at `a[a]`.
   - `Insert { b, len }`: `len >= 1` lines of `b` starting at `b[b]`.
3. Consequently the normalized script is a sequence in which every changed run is
   `Delete` (optional) immediately followed by `Insert` (optional), and changed runs are
   separated by `Equal` runs. `myers(a, b)` returns exactly this sequence; for `N = M = 0` it is
   empty.

## 5. Hunks

`diff_lines(a, b)` turns each changed run of the normalized script into one
`Hunk { base: x0..x0+nd, new: y0..y0+ni }`, where `(x0, y0)` is the position before the run,
`nd` the number of deleted lines and `ni` the number of inserted lines. Hunks are returned in
ascending order. A pure insertion has an empty base range (`x0..x0`); a pure deletion has an
empty new range. Two hunks of the same `diff_lines` result are always separated by at least one
equal line, so their base ranges never overlap or touch.

## 6. Three-way merge (`merge3`)

Input: byte strings `base`, `ours`, `theirs`. Let `B`, `O`, `T` be their `split_lines`.

1. **Hunks.** `HO = diff_lines(B, O)` (side `ours`), `HT = diff_lines(B, T)` (side `theirs`).
2. **Ordering.** Put all hunks of both sides in one list sorted by `(base.start, base.end)`
   (ties between sides in any order; the result does not depend on it).
3. **Regions.** Sweep the sorted list, maintaining the current region and its end
   `E = max(base.end)` over its hunks. A hunk `h` joins the current region iff
   `h.base.start <= E`; otherwise it starts a new region. This computes the transitive closure
   of "base ranges overlap or touch", where `[s1, e1)` and `[s2, e2)` with `s1 <= s2` touch iff
   `s2 <= e1`. Hence:
   - ranges that share a line overlap;
   - adjacent ranges (`e1 == s2`, zero unchanged lines between them) touch;
   - an empty insertion range `[s, s)` touches a range `[s2, e2)` when `s2 <= s <= e2`, i.e.
     when it lies at either end of the range or inside it.
   A region's base range is `[rs, re)` with `rs` = smallest `base.start`, `re` = largest
   `base.end` of its hunks.
4. **Side text of a region.** For side `S` (with lines `L_S`) having at least one hunk in the
   region, its hunks in the region in ascending order `h_1 .. h_n`:
   ```text
   text_S = B[rs .. h_1.base.start] ++ L_S[h_1.new] ++ B[h_1.base.end .. h_2.base.start]
            ++ L_S[h_2.new] ++ ... ++ L_S[h_n.new] ++ B[h_n.base.end .. re]
   range_S = (h_1.new.start - (h_1.base.start - rs)) .. (h_n.new.end + (re - h_n.base.end))
   ```
   `range_S` is the line range of `text_S` within `L_S`.
5. **Decision per region.**
   - Hunks from one side only: the region is replaced by that side's `text_S` (applied).
   - Hunks from both sides and `text_ours == text_theirs` (as line sequences, hence bytes): the
     region is replaced by that text once.
   - Otherwise the region is a conflict, reported as
     `ConflictHunk { base: rs..re, ours: range_ours, theirs: range_theirs }` (u32 line indices).
6. **Assembly.** Output, in order: the base lines before the first region, each region's
   replacement, the base lines between regions, and the base lines after the last region. If any
   region conflicted the result is `Merge3::Conflict(all conflict hunks in region order)`;
   otherwise `Merge3::Clean(concatenated bytes)`. The output is byte-exact: lines are copied
   verbatim, so terminators, `\r` bytes and an unterminated final line are preserved exactly as
   they appear in the side that supplied them.

Consequences worth stating: when `ours == base` the result is `theirs` exactly, and vice versa;
when `ours == theirs` the result is `ours`; merging is symmetric up to swapping the `ours` and
`theirs` ranges of conflict hunks.

## 7. Where content merge sits in SPEC 8.3

`merge3` is invoked only for the "otherwise" row of the SPEC 8.3 table, i.e. after
`classify` (rows 1-6: `o == b`, `t == b`, `o == t`, add_add, modify_delete, symlink) returned
`NeedContent`, and after the caller ruled out `redacted` (any needed side's content redacted) and
`binary` (any side contains `0x00` within its first 8000 bytes, or is larger than
`limits.merge_max_text_bytes`). The merged mode uses the first three rows applied to modes
(`merge_mode`). A `Conflict` result becomes a `content` conflict carrying the hunks. Conflict
markers are NEVER written into server-side objects.

## 8. Client-side marker rendering (SPEC 11.7)

`render_markers(base, ours, theirs)` produces the working-copy file for a `content` conflict. It
runs `merge3`; regions that resolved are emitted as in section 6.6, and every conflicting region
is emitted as:

```text
<<<<<<< ours\n
<text_ours lines>
||||||| base\n
<base lines rs..re>
=======\n
<text_theirs lines>
>>>>>>> theirs\n
```

If the content preceding a marker line is non-empty and does not end in `\n`, a single `\n` is
emitted before the marker line so that every marker starts at the beginning of a line.

## 9. Line origins and unified hunks

- `line_origins(parent, child)`: for each line of `child`, `Some(i)` when the normalized script
  `myers(split_lines(parent), split_lines(child))` maps it through an `Equal` run to `parent`
  line `i`, otherwise `None`. Used by blame (SPEC 9.2).
- `unified_hunks(a, b, context)`: group the changed runs of the normalized script; two changed
  runs belong to the same hunk when at most `2 * context` equal lines separate them. Each hunk
  covers its runs plus up to `context` equal lines before and after. `old_start` / `new_start`
  are 1-based line numbers of the first covered line; when a side covers zero lines its start is
  the number of the line after which the change occurs (0 at the beginning), as in GNU diff.
  `lines` lists covered lines in order, prefixed `b' '` (equal), `b'-'` (deleted) or `b'+'`
  (inserted), deletions of a run before its insertions.

## 10. Worked examples (normative)

All examples were computed by a direct transcription of sections 1-6. Lines are shown as
escaped strings; `\n` and `\r` are single bytes.

### 10.1 Trace of D4 (`a = [A, B]`, `b = [B, A]`, `N = M = 2`)

```text
d = 0: k = 0: k == -d, so x = V[1] = 0, y = 0. a[0]="A\n" != b[0]="B\n". V[0] = 0.
d = 1: k = -1: k == -d, so x = V[0] = 0, y = 1. Snake: a[0] == b[1] -> (1, 2). V[-1] = 1.
       k = +1: k == d, so x = V[0] + 1 = 1, y = 0. Snake: a[1] == b[0] -> (2, 1). V[1] = 2.
d = 2: k = -2: k == -d, so x = V[-1] = 1, y = 3. No snake (y >= M). V[-2] = 1.
       k =  0: V[-1] = 1 < V[1] = 2, so x = V[1] = 2, y = 2. Done, D = 2.
Backtrack: (2,2) d=2: k=0, V_1[-1]=1 < V_1[1]=2 -> prev_k=1, prev=(2,1): INSERT b[1].
           (2,1) d=1: k=1 == d -> prev_k=0, prev=(0,0): EQUAL(a:1,b:0), then DELETE a[0].
Script: DELETE a[0], EQUAL(1,0), INSERT b[1]  ->  B is kept; A moves.
```

Had the selection preferred insertions on ties, the result would have kept `A` instead. The rule
of section 2 is therefore observable and frozen.

### 10.2 Diff examples

#### D1: empty inputs

```text
a      = []
b      = []
edits  = []
hunks  = []
```

#### D2: insert into empty

```text
a      = []
b      = ["x\n"]
edits  = [Insert{b:0, len:1}]
hunks  = [{base: 0..0, new: 0..1}]
```

#### D3: delete everything

```text
a      = ["x\n"]
b      = []
edits  = [Delete{a:0, len:1}]
hunks  = [{base: 0..1, new: 0..0}]
```

#### D4: swap AB/BA (tie: the deletion branch wins, so B is kept)

```text
a      = ["A\n", "B\n"]
b      = ["B\n", "A\n"]
edits  = [Delete{a:0, len:1}, Equal{a:1, b:0, len:1}, Insert{b:1, len:1}]
hunks  = [{base: 0..1, new: 0..0}, {base: 2..2, new: 1..2}]
```

#### D5: delete a middle line

```text
a      = ["A\n", "B\n", "C\n"]
b      = ["A\n", "C\n"]
edits  = [Equal{a:0, b:0, len:1}, Delete{a:1, len:1}, Equal{a:2, b:1, len:1}]
hunks  = [{base: 1..2, new: 1..1}]
```

#### D6: repeated lines: the LAST copy is deleted

```text
a      = ["a\n", "a\n", "a\n"]
b      = ["a\n", "a\n"]
edits  = [Equal{a:0, b:0, len:2}, Delete{a:2, len:1}]
hunks  = [{base: 2..3, new: 2..2}]
```

#### D7: repeated lines: an inserted copy goes at the END

```text
a      = ["x\n"]
b      = ["x\n", "x\n"]
edits  = [Equal{a:0, b:0, len:1}, Insert{b:1, len:1}]
hunks  = [{base: 1..1, new: 1..2}]
```

#### D8: rotation of a repeated pair

```text
a      = ["a\n", "b\n", "a\n", "b\n"]
b      = ["b\n", "a\n", "b\n", "a\n"]
edits  = [Delete{a:0, len:1}, Equal{a:1, b:0, len:3}, Insert{b:3, len:1}]
hunks  = [{base: 0..1, new: 0..0}, {base: 4..4, new: 3..4}]
```

#### D9: unterminated last line differs from the terminated one

```text
a      = ["x\n", "y"]
b      = ["x\n", "y\n"]
edits  = [Equal{a:0, b:0, len:1}, Delete{a:1, len:1}, Insert{b:1, len:1}]
hunks  = [{base: 1..2, new: 1..2}]
```

#### D10: CRLF lines differ from LF lines

```text
a      = ["l1\r\n", "l2\r\n"]
b      = ["l1\n", "l2\r\n"]
edits  = [Delete{a:0, len:1}, Insert{b:0, len:1}, Equal{a:1, b:1, len:1}]
hunks  = [{base: 0..1, new: 0..1}]
```

#### D11: replacement: deletions listed before insertions

```text
a      = ["a\n", "b\n"]
b      = ["c\n", "d\n"]
edits  = [Delete{a:0, len:2}, Insert{b:0, len:2}]
hunks  = [{base: 0..2, new: 0..2}]
```

#### D12: Myers paper example ABCABBA -> CBABAC

```text
a      = ["A\n", "B\n", "C\n", "A\n", "B\n", "B\n", "A\n"]
b      = ["C\n", "B\n", "A\n", "B\n", "A\n", "C\n"]
edits  = [Delete{a:0, len:2}, Equal{a:2, b:0, len:1}, Insert{b:1, len:1}, Equal{a:3, b:2, len:2}, Delete{a:5, len:1}, Equal{a:6, b:4, len:1}, Insert{b:5, len:1}]
hunks  = [{base: 0..2, new: 0..0}, {base: 3..3, new: 1..2}, {base: 5..6, new: 4..4}, {base: 7..7, new: 5..6}]
```

### 10.3 Merge examples

#### M1: non-overlapping edits on both sides

```text
base   = ["a\n", "b\n", "c\n"]
ours   = ["A\n", "b\n", "c\n"]
theirs = ["a\n", "b\n", "C\n"]
result = Clean("A\nb\nC\n")
```

#### M2: same line changed differently: conflict

```text
base   = ["a\n", "b\n", "c\n"]
ours   = ["a\n", "X\n", "c\n"]
theirs = ["a\n", "Y\n", "c\n"]
result = Conflict([{base: 1..2, ours: 1..2, theirs: 1..2}])
```

#### M3: identical change on both sides: applied once

```text
base   = ["a\n", "b\n", "c\n"]
ours   = ["a\n", "X\n", "c\n"]
theirs = ["a\n", "X\n", "c\n"]
result = Clean("a\nX\nc\n")
```

#### M4: adjacent changed lines (zero unchanged lines between): conflict

```text
base   = ["a\n", "b\n", "c\n", "d\n"]
ours   = ["a\n", "B\n", "c\n", "d\n"]
theirs = ["a\n", "b\n", "C\n", "d\n"]
result = Conflict([{base: 1..3, ours: 1..3, theirs: 1..3}])
```

#### M5: separated by one unchanged line: clean

```text
base   = ["1\n", "2\n", "3\n", "4\n", "5\n"]
ours   = ["1\n", "X\n", "3\n", "4\n", "5\n"]
theirs = ["1\n", "2\n", "3\n", "Y\n", "5\n"]
result = Clean("1\nX\n3\nY\n5\n")
```

#### M6: insertions at different points

```text
base   = ["a\n", "b\n"]
ours   = ["a\n", "X\n", "b\n"]
theirs = ["a\n", "b\n", "Y\n"]
result = Clean("a\nX\nb\nY\n")
```

#### M7: one-sided change only

```text
base   = ["a\n", "b\n"]
ours   = ["X\n", "a\n", "b\n"]
theirs = ["a\n", "b\n"]
result = Clean("X\na\nb\n")
```

#### M8: both sides add different content to an empty base: conflict

```text
base   = []
ours   = ["x\n"]
theirs = ["y\n"]
result = Conflict([{base: 0..0, ours: 0..1, theirs: 0..1}])
```

#### M9: both sides add identical content to an empty base

```text
base   = []
ours   = ["x\n"]
theirs = ["x\n"]
result = Clean("x\n")
```

#### M10: an insertion touching the end of a deletion: conflict

```text
base   = ["a\n", "b\n", "c\n"]
ours   = ["a\n", "c\n"]
theirs = ["a\n", "b\n", "B\n", "c\n"]
result = Conflict([{base: 1..2, ours: 1..1, theirs: 1..3}])
```

#### M11: unterminated last line gains a terminator on one side

```text
base   = ["a\n", "b"]
ours   = ["a\n", "b\n"]
theirs = ["a\n", "b"]
result = Clean("a\nb\n")
```

#### M12: CRLF preserved byte for byte

```text
base   = ["a\r\n", "b\r\n"]
ours   = ["a\n", "b\r\n"]
theirs = ["a\r\n", "b\r\n", "c\r\n"]
result = Clean("a\nb\r\nc\r\n")
```

