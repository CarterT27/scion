#!/usr/bin/env python3
"""Concurrent CI workloads against a pinned Git mirror and a production Scion server.

Runs sequential, randomized waves: implementations never compete with each other.
Git uses a ready shared mirror (shared clones), plus fresh shallow/filtered clones.
Scion uses direct access and optionally one shared, prewarmed node daemon.
All setup needed by each checkout job is timed; fixture/service setup and validation are not.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
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
                if line.startswith(("scion_storage_", "scion_cache_requests_total", "scion_upstream_requests_total", "scion_object_bytes_served_total", "scion_rpc_duration_seconds_count"))}


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
            "wave_range_seconds": [min(w["seconds"] for w in waves), max(w["seconds"] for w in waves)]}


def self_test():
    assert percentile([4, 1, 3, 2], .95) == 4
    assert summarize([{"seconds": 2, "jobs": [{"seconds": 1}, {"seconds": 2}]}])["median_jobs_per_second"] == 1
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
    p.add_argument("--server-pid", type=int)
    p.add_argument("--daemon-pid", type=int)
    p.add_argument("--cache-dir", type=Path)
    p.add_argument("--containers", default="", help="Comma-separated dedicated backend Docker containers; requires cgroup v2.")
    p.add_argument("--concurrency", default="1,4,8")
    p.add_argument("--scopes", default="apps/web/src/lib,apps/web")
    p.add_argument("--runs", type=int, default=3)
    p.add_argument("--warmups", type=int, default=2)
    p.add_argument("--workloads", default="checkout,query,commit")
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
    if set(workloads) - {"checkout", "query", "commit"}:
        p.error("workloads: checkout,query,commit")
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
                  "Scion commits remain in the configured server as unreferenced changesets; main is never changed."]}
    roots = set()
    mirror = work / "receiver.git"

    def save():
        a.output.write_text(json.dumps(result, indent=2) + "\n")

    def command(args, mode="git-mirror", measured=False, resources=None):
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

    def wave(mode, workload, scope, concurrency, trial):
        ensure_idle()
        paths = [work / f"job-{workload}-{mode}-{trial}-{concurrency}-{i}" for i in range(concurrency)]
        roots.update((root, mode) for root in paths)
        edit = next(x for x, (m, _) in expected.items() if x.startswith(scope + "/") and m == b"100644")
        changed = None
        if workload == "commit":
            def prepare(root):
                checkout(mode, root, scope)
                validate(root, scope, mode == "git-sparse-index")
            with ThreadPoolExecutor(max_workers=min(concurrency, 8)) as pool:
                list(pool.map(prepare, paths))
            original = (paths[0] / edit).read_bytes()
            nonce = os.urandom(16).hex()
            changed = {root: original + f"\nbenchmark {nonce} {i:04d}\n".encode() for i, root in enumerate(paths)}
            for root, content in changed.items():
                (root / edit).write_bytes(content)
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
            elif mode in endpoints:
                out = command([binary, "-C", root, "--json", "commit", "-m", "benchmark one-file change"], mode, True, resources)
            else:
                command(git(root, "-c", "core.sparseCheckout=true", "commit", "-a", "-m", "benchmark one-file change"), mode, True, resources)
                ref = f"refs/heads/{root.name}"
                command(git(root, "push", str(mirror), f"HEAD:{ref}"), mode, True, resources)
                out = ref
            return {"seconds": time.perf_counter() - start, **resources}, out
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
        for root, (_, out) in zip(paths, completed):
            if workload == "checkout":
                result["validation"][f"{mode}:{scope}:files"] = validate(root, scope, mode == "git-sparse-index")
                measured.setdefault("materialized_bytes", []).append(sum(x.lstat().st_size for x in root.rglob("*") if (x.is_file() or x.is_symlink()) and not ({".git", ".scion"} & set(x.relative_to(root).parts))))
                if mode in ("git-partial", "git-shallow"):
                    measured.setdefault("received_pack_bytes", []).append(sum(x.stat().st_size for x in (root / ".git/objects/pack").glob("*.pack")))
            elif workload == "query":
                assert out == (query_bytes, query_messages), "file bytes or first-parent history differ"
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
                if workload == "query" and scope != scopes[0]:
                    continue
                active = modes if workload == "checkout" else [m for m in modes if m not in ("git-partial", "git-shallow")]
                if workload == "query":
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
