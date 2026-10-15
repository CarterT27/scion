#!/usr/bin/env python3
"""Concurrent CI workloads against a pinned Git mirror and a production Scion server.

Runs sequential, randomized waves: implementations never compete with each other.
Git uses a ready shared mirror (shared clones), plus fresh shallow/filtered clones.
Scion uses direct access and optionally one shared, prewarmed node daemon.
All setup needed by each checkout job is timed; fixture/service setup and validation are not.
"""
import argparse
import collections
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import re
import runpy
import shutil
import statistics
import subprocess
import threading
import time
import urllib.request

helpers = runpy.run_path(str(Path(__file__).with_name("benchmark-git.py")))


def percentile(values, q):
    return sorted(values)[max(0, math.ceil(len(values) * q) - 1)]


def ensure_idle():
    processes = subprocess.check_output(["ps", "-axo", "comm="], text=True).splitlines()
    if any(Path(x.strip()).name in {"cargo", "rustc", "rust-lld"} or "/target/debug/deps/" in x for x in processes):
        raise RuntimeError("Build or test processes are running; retry after they finish so timings are not contaminated.")


def snapshot(url):
    with urllib.request.urlopen(url, timeout=10) as response:
        return {line.rsplit(" ", 1)[0]: float(line.rsplit(" ", 1)[1])
                for line in response.read().decode().splitlines()
                if line.startswith(("scion_storage_", "scion_cache_requests_total", "scion_upstream_requests_total", "scion_object_bytes_served_total", "scion_rpc_duration_seconds_count",
                                    "scion_land_attempts", "scion_ref_cas_failures_total"))}


def hit_ratio(delta):
    """Daemon cache hit ratio by request count from a `scion_cache_requests_total` delta."""
    hits = sum(v for k, v in delta.items() if k.startswith("scion_cache_requests_total") and 'result="hit"' in k)
    misses = sum(v for k, v in delta.items() if k.startswith("scion_cache_requests_total") and 'result="miss"' in k)
    return hits / (hits + misses) if hits + misses else None


def container_values(names):
    values = {}
    for name in names:
        lines = subprocess.check_output(["docker", "exec", name, "cat", "/sys/fs/cgroup/cpu.stat", "/sys/fs/cgroup/memory.current"], text=True).splitlines()
        cpu = next(int(line.split()[1]) for line in lines if line.startswith("usage_usec ")) / 1_000_000
        values[name] = {"cpu_seconds": cpu, "memory_current_bytes": int(lines[-1])}
    return values


def summarize(waves):
    latencies = [j["seconds"] for w in waves for j in w["jobs"]]
    return {"median_wave_seconds": statistics.median(w["seconds"] for w in waves),
            "median_jobs_per_second": statistics.median(len(w["jobs"]) / w["seconds"] for w in waves),
            "job_p50_seconds": statistics.median(latencies), "job_p95_seconds": percentile(latencies, .95),
            "median_accounted_cpu_seconds": statistics.median(w["accounted_cpu_seconds"] for w in waves) if all(w.get("accounted_cpu_seconds") is not None for w in waves) else None,
            "wave_range_seconds": [min(w["seconds"] for w in waves), max(w["seconds"] for w in waves)],
            **({"median_daemon_hit_ratio": statistics.median(w["daemon_hit_ratio"] for w in waves)}
               if all(w.get("daemon_hit_ratio") is not None for w in waves) else {}),
            **({"median_push_retries_per_job": statistics.median(j.get("push_retries", 0) for w in waves for j in w["jobs"])}
               if any("push_retries" in j for w in waves for j in w["jobs"]) else {})}


def self_test():
    assert percentile([4, 1, 3, 2], .95) == 4
    assert summarize([{"seconds": 2, "jobs": [{"seconds": 1}, {"seconds": 2}]}])["median_jobs_per_second"] == 1
    assert hit_ratio({'scion_cache_requests_total{tier="daemon",kind="blob",result="hit"}': 3,
                      'scion_cache_requests_total{tier="daemon",kind="blob",result="miss"}': 1}) == .75
    assert hit_ratio({}) is None
    print("scaling benchmark checks passed")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--self-test", action="store_true")
    p.add_argument("--repo", type=Path)
    p.add_argument("--scion", type=Path)
    p.add_argument("--endpoint")
    p.add_argument("--daemon-endpoint")
    p.add_argument("--ca-file", type=Path)
    p.add_argument("--token-file", type=Path)
    p.add_argument("--work-dir", type=Path)
    p.add_argument("--output", type=Path)
    p.add_argument("--metrics-url", default="http://127.0.0.1:19465/metrics")
    p.add_argument("--daemon-metrics-url", help="Daemon /metrics, for the cache hit ratio of daemon waves.")
    p.add_argument("--loadgen", type=Path, help="scion-conformance loadgen example; required by land and history.")
    p.add_argument("--server-pid", type=int)
    p.add_argument("--daemon-pid", type=int)
    p.add_argument("--cache-dir", type=Path)
    p.add_argument("--containers", default="", help="Comma-separated dedicated backend Docker containers; requires cgroup v2.")
    p.add_argument("--concurrency", default="1,4,8")
    p.add_argument("--scopes", default="apps/web/src/lib,apps/web")
    p.add_argument("--runs", type=int, default=3)
    p.add_argument("--warmups", type=int, default=2)
    p.add_argument("--workloads", default="checkout,query,history,commit,land")
    p.add_argument("--skip-partial", action="store_true")
    a = p.parse_args()
    if a.self_test:
        self_test()
        return
    for key in ["repo", "scion", "endpoint", "ca_file", "token_file", "work_dir", "output"]:
        if getattr(a, key) is None:
            p.error(f"--{key.replace('_', '-')} is required")
    counts = [int(x) for x in a.concurrency.split(",")]
    scopes = a.scopes.split(",")
    workloads = a.workloads.split(",")
    containers = [name for name in a.containers.split(",") if name]
    if a.runs < 1 or a.warmups < 0 or not counts or min(counts) < 1 or max(counts) > 64:
        p.error("positive runs and concurrency in 1..64 required")
    if any(not s or s.startswith("/") or any(c in ("", ".", "..") for c in s.split("/")) for s in scopes):
        p.error("scopes must be repository-relative directories")
    if set(workloads) - {"checkout", "query", "history", "commit", "land"}:
        p.error("workloads: checkout,query,history,commit,land")
    if {"history", "land"} & set(workloads) and a.loadgen is None:
        p.error("--loadgen is required by the history and land workloads")
    ensure_idle()
    repo, binary, work = a.repo.resolve(), a.scion.resolve(), a.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=False)
    a.output.parent.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null", SCION_TOKEN=a.token_file.read_text().strip(),
               SCION_CA_FILE=str(a.ca_file.resolve()), SCION_AUTHOR="Benchmark <benchmark@example.com>",
               GIT_AUTHOR_NAME="Benchmark", GIT_AUTHOR_EMAIL="benchmark@example.com",
               GIT_COMMITTER_NAME="Benchmark", GIT_COMMITTER_EMAIL="benchmark@example.com")
    endpoints = {"direct": a.endpoint}
    if a.daemon_endpoint:
        endpoints["daemon"] = a.daemon_endpoint
    modes = ["git-mirror", "git-sparse-index", *endpoints]
    if not a.skip_partial:
        modes.insert(1, "git-partial")
        modes.insert(1, "git-shallow")
    randomizer = random.Random(42)
    result = {"method": __doc__, "os": platform.platform(), "concurrency": counts, "runs": a.runs,
              "waves": {}, "summary": {}, "validation": {}, "setup": {}, "warmups": a.warmups,
              "client_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "workspace_revision_at_run": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(), "containers": containers, "limitations": [
                  "Single machine and one authoritative Scion server; concurrency scaling, not horizontal scaling.",
                  "Operating-system caches are warm; daemon priming is recorded separately, not called cold.",
                  "CPU includes Git child processes and Scion client/server/daemon; dedicated backend CPU is included only with --containers.",
                  "Container CPU snapshots include background work and small observer overhead; container memory includes page cache and is sampled only at wave boundaries.",
                  "RPC payload counters exclude transport framing and TLS; Git pack bytes are on-disk received sizes, not NIC bytes.",
                  "Git shared-clone jobs borrow objects from a ready local mirror; partial clones transfer their own objects.",
                  "git-sparse-index uses cone mode and includes ancestor-directory files; its extra files and bytes are reported.",
                  "Git commits use default durability and push private refs into a disposable local receiver; Scion retains production durability.",
                  "Scion commits remain in the configured server as unreferenced changesets; main is never changed.",
                  "Land waves land on a fresh per-wave ref (Scion) or branch (Git receiver) created at the pinned commit; each job changes a distinct file, so Git retries are push races, not content conflicts.",
                  "Git land jobs retry rejected pushes with fetch + rebase onto the shared branch; Scion lands server-side with its own lock-free retries.",
                  "History queries run against a ready Git mirror and the Scion server or daemon; the Scion side spawns one loadgen process per query."]}
    roots = set()
    mirror = work / "receiver.git"

    def save():
        a.output.write_text(json.dumps(result, indent=2) + "\n")

    def command(args, mode="git-mirror", measured=False, resources=None, check=True):
        cmd = [str(x) for x in args]
        local = env.copy()
        if mode in endpoints:
            local["SCION_ENDPOINT"] = endpoints[mode]
        resource = None
        if measured and platform.system() == "Darwin":
            # Unique files without a shared counter in concurrent workers.
            resource = work / f"resource-{threading.get_ident()}-{time.monotonic_ns()}.txt"
            cmd = ["/usr/bin/time", "-l", "-o", str(resource), *cmd]
        done = subprocess.run(cmd, env=local, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if resource is not None and resources is not None:
            for k, v in helpers["resource_values"](resource.read_text()).items():
                resources[k] = max(resources.get(k, 0), v) if "rss" in k else resources.get(k, 0) + v
        if not check:
            return done
        if done.returncode:
            raise RuntimeError(f"{args}: {done.returncode}: {done.stderr.decode(errors='replace')}")
        return done.stdout

    def git(root, *args):
        return ["git", "-C", root, "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false",
                "-c", "diff.ignoreSubmodules=all", *args]

    sha = command(["git", "--git-dir", repo, "rev-parse", "HEAD"]).decode().strip()
    result["snapshot"] = sha
    result["commits"] = int(command(["git", "--git-dir", repo, "rev-list", "--count", sha]))
    entries = command(["git", "--git-dir", repo, "ls-tree", "-rz", sha])
    expected = {}
    for entry in filter(None, entries.split(b"\0")):
        info, path = entry.split(b"\t", 1)
        mode, kind, oid = info.split()
        if kind == b"blob":
            expected[os.fsdecode(path)] = (mode, oid)
    for scope in scopes:
        assert any(x.startswith(scope + "/") for x in expected), f"empty scope {scope}"
    result["files"] = len(expected)
    base = json.loads(command([binary, "--json", "show", "main"], "direct"))["data"]["id"]
    assert base == json.loads(command([binary, "--json", "show", f"git:{sha}"], "direct"))["data"]["id"]
    started = time.perf_counter()
    # All benchmark refs/worktree bookkeeping are isolated from the user's source mirror.
    command(["git", "clone", "--bare", "--shared", "--single-branch", "--no-tags", repo, mirror])
    command(["git", "--git-dir", mirror, "commit-graph", "write", "--reachable"])
    result["setup"]["git_receiver_seconds"] = time.perf_counter() - started
    result["setup"]["git_commit_graph"] = True
    result["setup"]["git_receiver_refs"] = len(command(["git", "--git-dir", mirror, "for-each-ref", "--format=%(refname)"]).splitlines())
    result["setup"]["git_mirror_object_bytes"] = sum(x.stat().st_size for x in (repo / "objects").rglob("*") if x.is_file())

    def checkout(mode, root, scope, resources=None, measured=False):
        if mode in endpoints:
            command([binary, "checkout", base, root, "--sparse", scope], mode, measured, resources)
            return
        if mode == "git-sparse-index":
            command(["git", "clone", "--shared", "--sparse", "--single-branch", "--no-tags", mirror, root], mode, measured, resources)
            command(git(root, "sparse-checkout", "set", "--cone", "--sparse-index", scope), mode, measured, resources)
            return
        if mode in ("git-partial", "git-shallow"):
            command(["git", "clone", "--no-checkout", "--single-branch", "--no-tags", "--filter=blob:none",
                     *(["--depth=1"] if mode == "git-shallow" else []),
                     "--upload-pack=git -c uploadpack.allowFilter=true upload-pack", repo.as_uri(), root], mode, measured, resources)
        else:
            command(["git", "clone", "--shared", "--no-checkout", "--single-branch", "--no-tags", mirror, root], mode, measured, resources)
        # Exact directory scope: no cone-mode inclusion of unrelated ancestor files.
        sparse_file = root / ".git/info/sparse-checkout"
        sparse_file.parent.mkdir(parents=True, exist_ok=True)
        sparse_file.write_text(f"/{scope}/\n")
        command(git(root, "-c", "core.sparseCheckout=true", "-c", "core.sparseCheckoutCone=false", "reset", "--hard", sha), mode, measured, resources)

    def validate(root, scope, cone=False):
        ancestors = {"/".join(scope.split("/")[:i]) for i in range(len(scope.split("/")))} if cone else set()
        want = {x: v for x, v in expected.items()
                if x.startswith(scope + "/") or x.rpartition("/")[0] in ancestors}
        actual = {str(x.relative_to(root)) for x in root.rglob("*")
                  if not ({".git", ".scion"} & set(x.relative_to(root).parts)) and (x.is_file() or x.is_symlink())}
        assert actual == set(want), (root, "scope mismatch")
        for name, (mode, oid) in want.items():
            path = root / name
            data = os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()
            assert hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest().encode() == oid, name
            assert path.is_symlink() == (mode == b"120000"), name
            if not path.is_symlink():
                assert bool(path.stat().st_mode & 0o100) == (mode == b"100755"), name
        return len(want)

    def cleanup(root, mode):
        if root.exists():
            shutil.rmtree(root)
        roots.discard((root, mode))

    def process_snapshot():
        return {name: helpers["process_values"](pid) for name, pid in [("server", a.server_pid), ("daemon", a.daemon_pid)] if pid}

    hist = {}

    def history_setup():
        """Pick the history queries once and compute Git's answers (untimed)."""
        def git_out(*args):
            return command(["git", "--git-dir", repo, *args]).decode(errors="replace")
        def scion_id(git_sha):
            return json.loads(command([binary, "--json", "show", f"git:{git_sha}"], "direct"))["data"]["id"]
        text = (".ts", ".tsx", ".js", ".json", ".md", ".css", ".yml", ".yaml", ".rs", ".py")
        changes = collections.Counter(git_out("log", "--format=", "--name-only", "-n", "2000", sha).splitlines())
        path = next(x for x, _ in changes.most_common() if x in expected and expected[x][0] == b"100644" and x.endswith(text))
        content = command(["git", "--git-dir", repo, "show", f"{sha}:{path}"])
        porcelain = git_out("blame", "--porcelain", sha, "--", path)
        old = git_out("rev-parse", f"{sha}~100").strip()
        merge = git_out("rev-list", "--merges", "--first-parent", "-n", "1", sha).strip()
        p1, p2 = git_out("rev-parse", f"{merge}^1", f"{merge}^2").split()
        merge_base = git_out("merge-base", p1, p2).strip()
        raw = git_out("diff", "--raw", "--no-renames", old, sha)
        hist.update({
            "blame_path": path,
            "blame_lines": content.count(b"\n") + (1 if content and not content.endswith(b"\n") else 0),
            "git_blame_origins": len({x.split()[0] for x in porcelain.splitlines() if re.match(r"^[0-9a-f]{40} ", x)}),
            "diff_range": [old, sha],
            "diff_paths": sorted(x.split("\t", 1)[1] for x in raw.splitlines()
                                 if x.startswith(":") and "160000" not in x[1:].split()[:2]),
            "merge": merge, "merge_base": merge_base, "merge_base_scion": scion_id(merge_base),
            "scion": [("blame", ["blame", base, path]), ("diff", ["diff", scion_id(old), base]),
                      ("merge_base", ["merge-base", scion_id(p1), scion_id(p2)])],
            "git": [("blame", ["blame", "--porcelain", sha, "--", path]), ("diff", ["diff", "--raw", "--no-renames", old, sha]),
                    ("merge_base", ["merge-base", p1, p2])],
        })
        result["setup"]["history_queries"] = {k: v for k, v in hist.items() if k not in ("scion", "git", "diff_paths")}
        result["setup"]["history_queries"]["diff_paths"] = len(hist["diff_paths"])

    def check_history(mode, outs):
        if mode in endpoints:
            blame, diff, merge_base = outs
            assert blame["lines"] == hist["blame_lines"], ("blame line count", blame["lines"], hist["blame_lines"])
            result["validation"].setdefault("history_blame_origins", {})[mode] = {"scion": len(blame["origins"]), "git": hist["git_blame_origins"]}
            assert sorted(diff["paths"]) == hist["diff_paths"], "diff paths differ from git"
            assert merge_base["bases"] == [hist["merge_base_scion"]], "merge base differs from git"
        else:
            blame, diff, merge_base = (x.decode(errors="replace") for x in outs)
            assert sum(1 for x in blame.splitlines() if x.startswith("\t")) == hist["blame_lines"]
            assert merge_base.strip() == hist["merge_base"]

    def wave(mode, workload, scope, concurrency, trial):
        ensure_idle()
        paths = [work / f"job-{workload}-{mode}-{trial}-{concurrency}-{i}" for i in range(concurrency)]
        roots.update((root, mode) for root in paths)
        edit = next(x for x, (m, _) in expected.items() if x.startswith(scope + "/") and m == b"100644")
        edits = {root: edit for root in paths}
        changed = None
        land_ref = None
        if workload in ("commit", "land"):
            if workload == "land":
                # Distinct files: concurrent landings are independent changes, as most are.
                files = sorted(x for x, (m, _) in expected.items() if x.startswith(scope + "/") and m == b"100644")
                assert len(files) >= concurrency, "scope has too few files for land concurrency"
                edits = dict(zip(paths, files))
                land_ref = (f"bench/land-{trial}-c{concurrency}-{mode}" if mode in endpoints
                            else f"refs/heads/bench-land-{trial}-c{concurrency}-{mode}")
            def prepare(root):
                checkout(mode, root, scope)
                validate(root, scope, mode == "git-sparse-index")
                if workload == "land" and mode == "git-mirror":
                    # Keep later rebases inside the same exact-directory sparse pattern.
                    command(git(root, "config", "core.sparseCheckout", "true"))
                    command(git(root, "config", "core.sparseCheckoutCone", "false"))
            with ThreadPoolExecutor(max_workers=min(concurrency, 8)) as pool:
                list(pool.map(prepare, paths))
            nonce = os.urandom(16).hex()
            changed = {root: (root / edits[root]).read_bytes() + f"\nbenchmark {nonce} {i:04d}\n".encode()
                       for i, root in enumerate(paths)}
            for root, content in changed.items():
                (root / edits[root]).write_bytes(content)
            if workload == "land":
                if mode in endpoints:
                    command([a.loadgen, "ref-reset", land_ref, base], "direct")
                else:
                    command(["git", "--git-dir", mirror, "update-ref", land_ref, sha])
        query_path = "README.md"
        query_bytes = command(["git", "--git-dir", repo, "show", f"{sha}:{query_path}"])
        query_log_args = ["log", "--first-parent", "-n", "100", "--format=%B%x00", sha]
        query_messages = [m.strip(b"\n").decode() for m in command(["git", "--git-dir", repo, *query_log_args]).split(b"\0")[:-1]]
        barrier = threading.Barrier(concurrency)
        def job(i):
            root = paths[i]
            resources = {}
            barrier.wait()
            start = time.perf_counter()
            if workload == "checkout":
                checkout(mode, root, scope, resources, True)
                out = None
            elif workload == "query":
                if mode in endpoints:
                    content = command([binary, "cat", f"{base}:{query_path}"], mode, True, resources)
                    log = command([binary, "--json", "log", "--rev", base, "--first-parent", "--limit", "100"], mode, True, resources)
                    out = content, [e["meta"]["message"].strip("\n") for e in json.loads(log)["data"]["entries"]]
                else:
                    # Ready-mirror metadata service baseline: no clone or working copy.
                    content = command(["git", "--git-dir", mirror, "show", f"{sha}:{query_path}"], mode, True, resources)
                    log = command(["git", "--git-dir", mirror, *query_log_args], mode, True, resources)
                    out = content, [m.strip(b"\n").decode() for m in log.split(b"\0")[:-1]]
            elif workload == "history":
                outs = []
                for name, args in hist["scion" if mode in endpoints else "git"]:
                    started = time.perf_counter()
                    if mode in endpoints:
                        outs.append(json.loads(command([a.loadgen, "query", *args], mode, True, resources)))
                    else:
                        outs.append(command(["git", "--git-dir", mirror, *args], mode, True, resources))
                    resources[f"{name}_seconds"] = time.perf_counter() - started
                out = outs
            elif workload == "land":
                started = time.perf_counter()
                if mode in endpoints:
                    command([binary, "-C", root, "--json", "commit", "-m", "benchmark landed change"], mode, True, resources)
                    resources["commit_seconds"] = time.perf_counter() - started
                    out = command([binary, "-C", root, "--json", "land", "--ref", land_ref], mode, True, resources)
                else:
                    command(git(root, "-c", "core.sparseCheckout=true", "commit", "-a", "-q", "-m", "benchmark landed change"), mode, True, resources)
                    resources["commit_seconds"] = time.perf_counter() - started
                    retries = 0
                    # Without a merge queue, Git writers race: fetch + rebase and push again until accepted.
                    while command(git(root, "push", "-q", str(mirror), f"HEAD:{land_ref}"), mode, True, resources, check=False).returncode:
                        retries += 1
                        if retries > 200:
                            raise RuntimeError("git push kept being rejected")
                        command(git(root, "fetch", "-q", str(mirror), land_ref), mode, True, resources)
                        command(git(root, "-c", "core.sparseCheckout=true", "rebase", "-q", "FETCH_HEAD"), mode, True, resources)
                    resources["push_retries"] = retries
                    out = None
                resources["publish_seconds"] = time.perf_counter() - started - resources["commit_seconds"]
            elif mode in endpoints:
                out = command([binary, "-C", root, "--json", "commit", "-m", "benchmark one-file change"], mode, True, resources)
            else:
                command(git(root, "-c", "core.sparseCheckout=true", "commit", "-a", "-m", "benchmark one-file change"), mode, True, resources)
                ref = f"refs/heads/{root.name}"
                command(git(root, "push", str(mirror), f"HEAD:{ref}"), mode, True, resources)
                out = ref
            return {"seconds": time.perf_counter() - start, **resources}, out
        daemon_metrics = mode == "daemon" and a.daemon_metrics_url
        daemon_before = snapshot(a.daemon_metrics_url) if daemon_metrics else None
        before = snapshot(a.metrics_url)
        backends = container_values(containers)
        processes = process_snapshot()
        peaks = {k: v["rss_bytes"] for k, v in processes.items()}
        stop = threading.Event()
        def sample():
            while not stop.wait(.25):
                for name, values in process_snapshot().items():
                    peaks[name] = max(peaks.get(name, 0), values["rss_bytes"])
        watcher = threading.Thread(target=sample, daemon=True)
        if processes:
            watcher.start()
        start = time.perf_counter()
        try:
            with ThreadPoolExecutor(max_workers=concurrency) as pool:
                completed = list(pool.map(job, range(concurrency)))
            elapsed = time.perf_counter() - start
        finally:
            stop.set()
            if processes:
                watcher.join()
        after = snapshot(a.metrics_url)
        daemon_after = snapshot(a.daemon_metrics_url) if daemon_metrics else None
        ensure_idle()
        last = process_snapshot()
        backend_last = container_values(containers)
        measured = {"seconds": elapsed, "jobs": [j for j, _ in completed],
                    "server_metrics_delta": {k: v - before.get(k, 0) for k, v in after.items()},
                    "processes": {k: {"cpu_seconds": v["cpu_seconds"] - processes[k]["cpu_seconds"],
                                      "sampled_peak_rss_bytes": max(peaks[k], v["rss_bytes"])} for k, v in last.items()}}
        measured["backends"] = {k: {"cpu_seconds": v["cpu_seconds"] - backends[k]["cpu_seconds"],
                                   "max_observed_memory_bytes": max(v["memory_current_bytes"], backends[k]["memory_current_bytes"])} for k, v in backend_last.items()}
        measured["accounted_cpu_seconds"] = sum(j.get("client_user_seconds", 0) + j.get("client_system_seconds", 0) for j, _ in completed)
        if mode in endpoints:
            measured["accounted_cpu_seconds"] += sum(v["cpu_seconds"] for v in measured["processes"].values())
            measured["accounted_cpu_seconds"] += sum(v["cpu_seconds"] for v in measured["backends"].values())
        if any("client_user_seconds" not in j for j, _ in completed):
            measured["accounted_cpu_seconds"] = None
        assert all(v >= 0 for v in measured["server_metrics_delta"].values()), "counter reset"
        if daemon_metrics:
            measured["daemon_metrics_delta"] = {k: v - daemon_before.get(k, 0) for k, v in daemon_after.items()}
            measured["daemon_hit_ratio"] = hit_ratio(measured["daemon_metrics_delta"])
        if workload == "land":
            # Every job's change is on the ref exactly once, in a linear chain from the base.
            if mode in endpoints:
                entries = json.loads(command([binary, "--json", "log", "--rev", land_ref, "--first-parent",
                                              "--limit", str(concurrency + 1)], "direct"))["data"]["entries"]
                assert len(entries) == concurrency + 1 and entries[-1]["id"] == base, "landed history is not linear from the base"
                for root, content in changed.items():
                    assert command([binary, "cat", f"{land_ref}:{edits[root]}"], "direct") == content, "a landed change is missing"
                command([a.loadgen, "ref-delete", land_ref], "direct")
            else:
                assert int(command(["git", "--git-dir", mirror, "rev-list", "--count", f"{sha}..{land_ref}"])) == concurrency
                for root, content in changed.items():
                    assert command(["git", "--git-dir", mirror, "show", f"{land_ref}:{edits[root]}"]) == content, "a pushed change is missing"
                command(["git", "--git-dir", mirror, "update-ref", "-d", land_ref])
        for root, (_, out) in zip(paths, completed):
            if workload == "checkout":
                result["validation"][f"{mode}:{scope}:files"] = validate(root, scope, mode == "git-sparse-index")
                measured.setdefault("materialized_bytes", []).append(sum(x.lstat().st_size for x in root.rglob("*") if (x.is_file() or x.is_symlink()) and not ({".git", ".scion"} & set(x.relative_to(root).parts))))
                if mode in ("git-partial", "git-shallow"):
                    measured.setdefault("received_pack_bytes", []).append(sum(x.stat().st_size for x in (root / ".git/objects/pack").glob("*.pack")))
            elif workload == "query":
                assert out == (query_bytes, query_messages), "file bytes or first-parent history differ"
            elif workload == "history":
                check_history(mode, out)
            elif workload == "land":
                pass
            elif mode in endpoints:
                report = json.loads(out)["data"]
                assert report["parents"] == [base] and len(report["delta"]) == 1 and report["delta"][0]["path"] == edit
                assert command([binary, "cat", f'{report["changeset"]}:{edit}'], "direct") == changed[root]
            else:
                assert command(["git", "--git-dir", mirror, "show", f"{out}:{edit}"]) == changed[root]
                assert command(["git", "--git-dir", mirror, "rev-parse", f"{out}^"]).decode().strip() == sha
                assert command(["git", "--git-dir", mirror, "diff-tree", "--no-commit-id", "--name-only", "-r", out]).decode().splitlines() == [edit]
                command(["git", "--git-dir", mirror, "update-ref", "-d", out])
            cleanup(root, mode)
        return measured

    try:
        if "history" in workloads:
            history_setup()
        for scope in scopes:
            if a.daemon_endpoint:
                root = work / "prime"
                roots.add((root, "daemon"))
                start = time.perf_counter()
                checkout("daemon", root, scope)
                result["setup"][f"daemon_prime_seconds:{scope}"] = time.perf_counter() - start
                validate(root, scope)
                cleanup(root, "daemon")
            for workload in workloads:
                if workload in ("query", "history") and scope != scopes[0]:
                    continue
                active = modes if workload == "checkout" else [m for m in modes if m not in ("git-partial", "git-shallow")]
                if workload in ("query", "history"):
                    active = [m for m in active if m != "git-sparse-index"]
                for mode in active:
                    for warmup in range(a.warmups):
                        wave(mode, workload, scope, 1, f"warmup-{warmup}")
                for concurrency in counts:
                    key = f"{workload}:{scope}:concurrency={concurrency}"
                    result["waves"][key] = {mode: [] for mode in active}
                    for trial in range(a.runs):
                        order = active.copy()
                        randomizer.shuffle(order)
                        for mode in order:
                            measured = wave(mode, workload, scope, concurrency, trial)
                            result["waves"][key][mode].append(measured)
                            save()
                            print(key, mode, trial, round(measured["seconds"], 4), "seconds", flush=True)
                    result["summary"][key] = {mode: summarize(waves) for mode, waves in result["waves"][key].items()}
                    save()
        assert json.loads(command([binary, "--json", "show", "main"], "direct"))["data"]["id"] == base
        result["validation"]["main_unchanged"] = True
        if a.cache_dir:
            result["setup"]["daemon_cache_bytes_after"] = sum(x.stat().st_size for x in a.cache_dir.rglob("*") if x.is_file())
        result["validation"]["all_waves_passed"] = True
        save()
        print(json.dumps(result["summary"], indent=2), flush=True)
    except Exception as error:
        result["failure"] = str(error)
        save()
        raise
    finally:
        for root, mode in list(roots):
            cleanup(root, mode)
        if mirror.exists():
            shutil.rmtree(mirror)


if __name__ == "__main__":
    main()
