export const meta = {
  name: 'scion-wave',
  description: 'Implement one wave of docs/PLAN.md work packages in parallel: one Opus agent per package in its own git worktree, followed by a fresh-agent review of each package',
  phases: [
    { title: 'Implement', detail: 'one agent per package, isolated worktree', model: 'opus' },
    { title: 'Review', detail: 'fresh-agent SPEC conformance review and fix per package', model: 'opus' },
  ],
}
// args: { wave: number, base: string (git sha of wave base on main), packages: [...PACKAGES.json entries for this wave] }
const REPO = '/Users/cartertran/Development/scion'
const { wave, base, ids, notes } = args
const packages = ids.map(id => ({ id }))

const REPORT = {
  type: 'object',
  properties: {
    package: { type: 'string' },
    worktree: { type: 'string', description: 'absolute path of the git worktree you worked in (output of pwd / git rev-parse --show-toplevel)' },
    branch: { type: 'string', description: 'output of git rev-parse --abbrev-ref HEAD in that worktree' },
    head: { type: 'string', description: 'git rev-parse HEAD after your final commit' },
    check_passed: { type: 'boolean', description: 'true only if scripts/check.sh printed "check.sh: all checks passed" on the final commit' },
    check_tail: { type: 'string', description: 'last ~30 lines of the final scripts/check.sh run' },
    summary: { type: 'string', description: 'what was implemented and tested; what is incomplete' },
    unowned_files_touched: { type: 'array', items: { type: 'string' }, description: 'files you changed that are outside your owns list (should be empty)' },
    amendments: { type: 'array', items: { type: 'string' }, description: 'PLAN 5.2 amendment requests: file, item, old signature, new signature, reason' },
  },
  required: ['package', 'worktree', 'branch', 'head', 'check_passed', 'check_tail', 'summary', 'unowned_files_touched', 'amendments'],
}

const COMMON = `You are implementing one work package of the Scion source control system (a Rust workspace). Repo: ${REPO}; wave base commit ${base} on main.
Required reading before writing code: docs/PLAN.md sections 0, 2.0, the PLAN section 2 subsections for the crates you own AND consume, sections 3 and 5, docs/implementation-defined.md, docs/contracts/*.md where relevant, and ALL the SPEC.md sections listed in your package entry (read them fully, plus SPEC section 18 reference algorithms and section 19 tests that apply). Existing code: the crates already contain frozen stubs (todo!("Pxx")) — you replace the bodies of stubs owned by your package and add private helpers and tests. Code owned by other packages in this wave is still todo!() — never call it at runtime in tests; use your package's depends_on list to know what is real.
HARD RULES:
- Edit ONLY files in your package's "owns" list (plus new private submodules/test files under those paths). Never edit any Cargo.toml, Cargo.lock, lib.rs, the proto, or another package's files. If you need a dependency or a frozen-signature change, STOP working around it: record an amendment request in your report and implement the rest.
- Frozen signatures in PLAN section 2 must stay byte-identical. Additive pub items inside your own files are fine.
- Library code: no unwrap/expect/panic (workspace lints deny them); tests may use them. thiserror in libs; tracing for logs. Rustdoc on pub items cites the SPEC section.
- Build setup: ${notes}
- Definition of done: scripts/check.sh passes in your worktree (fmt, build --all-targets, test, clippy -D warnings, layer check). Run "cargo fmt --all" before committing. Run the fast checks on just your crate first (cargo test -p <crate>) and the full scripts/check.sh at the end.
- Commit to git on your worktree branch at milestones and at the end (git add -A on your owned paths; git commit). Leave nothing uncommitted. Do NOT merge, rebase, push, or touch main.
- Scale: write the tests the package entry lists (SPEC 19.x). Spec-sized counts (10^6 cases etc.) go behind #[ignore] with a "full_" prefix and read SCION_FULL; default runs use scaled-down counts per PLAN 3.5.
Your final answer is a structured report consumed by an orchestrator, not a human.`

phase('Implement')
const results = await pipeline(
  packages,
  pkg => agent(`${COMMON}

YOUR PACKAGE: ${pkg.id} (wave ${wave}). Its full entry (title, spec_sections, owns, depends_on, deliverables, tests) is in docs/PACKAGES.json; extract it first with:
  python3 -c 'import json;[print(json.dumps(x,indent=1)) for x in json.load(open("docs/PACKAGES.json")) if x["id"]=="${pkg.id}"]'

Start by running: pwd; git rev-parse --abbrev-ref HEAD; git log --oneline -1   (you are in an isolated git worktree created for you; record the path and branch for your report). Then read, then implement everything in "deliverables" and "tests". Work through the deliverables in dependency order; commit as you go.`,
    { label: `${pkg.id}:impl`, phase: 'Implement', model: 'opus', effort: 'high', isolation: 'worktree', schema: REPORT }),
  (impl, pkg) => {
    if (!impl) { log(`${pkg.id}: implementer returned nothing`); return null }
    log(`${pkg.id}: implemented (check ${impl.check_passed ? 'PASS' : 'FAIL'}) on ${impl.branch}`)
    return agent(`${COMMON}

You are the REVIEWER for package ${pkg.id}. A previous agent implemented it in the git worktree at ${impl.worktree} on branch ${impl.branch} (head ${impl.head}). cd into that worktree and work there (do not create a new worktree). Its report:
${JSON.stringify(impl, null, 1)}

The package entry is in docs/PACKAGES.json (id ${pkg.id}); extract it with python3/json as the implementer did.

Tasks, in order:
1. Verify: git status clean; scripts/check.sh passes. If it does not, fix it first.
2. Adversarially compare the implementation against every SPEC.md section in the package entry and the reference algorithms in SPEC 18 and the frozen contracts in PLAN section 2 and docs/contracts. Look for: wrong edge-case behaviour, missing MUST requirements, missing or weak SPEC 19.x tests listed in the entry, leftover todo!() in files this package owns, unwrap/expect in library code, nondeterminism, O(repo) instead of O(delta) complexity where the SPEC demands it.
3. Fix every defect you find inside the package's owned files; add the missing tests. Keep frozen signatures intact. Do not edit unowned files.
4. Run scripts/check.sh; cargo fmt --all; commit on the same branch.
Report with the same structure; "summary" should list the defects found and fixed, and anything still incomplete.`,
      { label: `${pkg.id}:review`, phase: 'Review', model: 'opus', effort: 'high', schema: REPORT })
  }
)

const out = results.map((r, i) => r || { package: packages[i].id, check_passed: false, summary: 'agent returned null', amendments: [] })
log(`wave ${wave}: ${out.filter(r => r.check_passed).length}/${packages.length} packages pass check.sh`)
return out
