#!/usr/bin/env python3
"""Compare Git and Scion at a pinned commit. Uses disposable working copies, never flushes caches.

Example:
  scripts/benchmark-git.py --repo /tmp/repo.git --scion target/release/scion \
    --importer target/release/scion-import --endpoint https://127.0.0.1:18443 \
    --ca-file /tmp/ca.crt --token-file /tmp/token --work-dir /tmp/bench \
    --output /tmp/results.json --daemon-endpoint unix:///tmp/daemon.sock

macOS client CPU/RSS comes from /usr/bin/time -l. Server RSS is sampled, not an allocation limit.
Server storage counters count logical metadata calls, not individual SQL statements.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import re
import shutil
import statistics
import subprocess
import threading
import time
import urllib.request


def metric_values(text):
    values = {}
    for line in text.splitlines():
        if line.startswith(("scion_storage_", "scion_object_bytes_served_total", "scion_rpc_duration_seconds_count", "scion_upstream_requests_total")):
            key, value = line.rsplit(" ", 1)
            values[key] = float(value)
    return values


def resource_values(text):
    result = {}
    match = re.search(r"([\d.]+) real\s+([\d.]+) user\s+([\d.]+) sys", text)
    if match:
        result.update(client_user_seconds=float(match[2]), client_system_seconds=float(match[3]))
    match = re.search(r"(\d+)\s+maximum resident set size", text)
    if match:
        result["client_peak_rss_bytes"] = int(match[1])
    return result


def process_values(pid):
    if not pid:
        return {}
    out = subprocess.check_output(["ps", "-o", "time=,rss=", "-p", str(pid)], text=True).split()
    parts = out[0].split(":")
    seconds = float(parts[-1]) + 60 * int(parts[-2])
    if len(parts) == 3:
        seconds += 3600 * int(parts[0])
    return {"cpu_seconds": seconds, "rss_bytes": int(out[1]) * 1024}


def self_test():
    assert metric_values('scion_storage_access{kind="object_gets"} 7\n# comment') == {'scion_storage_access{kind="object_gets"}': 7.0}
    assert resource_values(' 1.23 real 0.12 user 0.34 sys\n 4096 maximum resident set size') == {"client_user_seconds": 0.12, "client_system_seconds": 0.34, "client_peak_rss_bytes": 4096}
    print("benchmark parser checks passed")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--self-test", action="store_true")
    p.add_argument("--repo", type=Path)
    p.add_argument("--scion", type=Path)
    p.add_argument("--importer", type=Path)
    p.add_argument("--endpoint")
    p.add_argument("--daemon-endpoint")
    p.add_argument("--ca-file", type=Path)
    p.add_argument("--token-file", type=Path)
    p.add_argument("--work-dir", type=Path)
    p.add_argument("--output", type=Path)
    p.add_argument("--metrics-url", default="http://127.0.0.1:19464/metrics")
    p.add_argument("--server-pid", type=int)
    p.add_argument("--checkout-runs", type=int, default=3)
    p.add_argument("--command-runs", type=int, default=9)
    p.add_argument("--clone-runs", type=int, default=3)
    p.add_argument("--sparse", default="apps/web")
    p.add_argument("--extended-only", action="store_true", help="Only daemon and sparse operations; retain the earlier direct benchmark separately.")
    a = p.parse_args()
    if a.self_test:
        self_test()
        return
    for name in ["repo", "scion", "endpoint", "ca_file", "token_file", "work_dir", "output"]:
        if getattr(a, name) is None:
            p.error(f"--{name.replace('_', '-')} is required")
    if a.checkout_runs < 1 or a.command_runs < 1 or a.clone_runs < 1:
        p.error("run counts must be positive")
    scope = a.sparse.strip("/")
    if not scope or any(s in ("", ".", "..") for s in scope.split("/")):
        p.error("--sparse must be a repository-relative directory")
    repo, binary = a.repo.resolve(), a.scion.resolve()
    work = a.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=False)
    a.output.parent.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null", SCION_CA_FILE=str(a.ca_file.resolve()), SCION_TOKEN=a.token_file.read_text().strip())
    modes = {"git": None, "direct": a.endpoint}
    if a.daemon_endpoint:
        modes["daemon"] = a.daemon_endpoint
    result = {"method": "Sequential paired trials; first checkout separated; no cache flush; 2 warmups then repeated commands. Git packed local storage; Scion local server storage, optionally daemon. Submodule contents excluded.", "os": platform.platform(), "git_version": subprocess.check_output(["git", "--version"], text=True).strip(), "samples": {}, "first_checkout": {}, "validation": {}, "limitations": ["Warm operating-system caches; first measurements are not controlled cold-cache results.", "Server storage counters are logical metadata calls and object-storage requests, not SQL statement counts.", "Server RSS is periodically sampled; client resource metrics use macOS /usr/bin/time."]}
    random.seed(42)
    seq = 0
    git_worktrees, disposable, sparse_roots = [], [], set()

    def save():
        a.output.write_text(json.dumps(result, indent=2) + "\n")

    def metrics():
        with urllib.request.urlopen(a.metrics_url, timeout=10) as response:
            return metric_values(response.read().decode())

    def run(cmd, mode="git", measured=False):
        nonlocal seq
        cmd = [str(x) for x in cmd]
        local_env = env.copy()
        if modes.get(mode):
            local_env["SCION_ENDPOINT"] = modes[mode]
        resource_file = work / f"resource-{seq}.txt"
        seq += 1
        actual = ["/usr/bin/time", "-l", "-o", str(resource_file), *cmd] if measured and platform.system() == "Darwin" else cmd
        before_metrics = metrics() if measured and mode != "git" else {}
        before_server = process_values(a.server_pid) if measured and mode != "git" else {}
        rss = [before_server.get("rss_bytes", 0)]
        stop = threading.Event()
        def sample():
            while not stop.wait(0.25):
                try:
                    rss.append(process_values(a.server_pid).get("rss_bytes", 0))
                except (subprocess.CalledProcessError, IndexError):
                    return
        watcher = threading.Thread(target=sample, daemon=True) if before_server else None
        if watcher:
            watcher.start()
        start = time.perf_counter()
        try:
            done = subprocess.run(actual, env=local_env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            seconds = time.perf_counter() - start
        finally:
            stop.set()
            if watcher:
                watcher.join()
        if done.returncode:
            raise RuntimeError(f"{cmd}: exit {done.returncode}: {done.stderr.decode(errors='replace')}")
        sample_result = {"seconds": seconds}
        if measured:
            if resource_file.exists():
                sample_result.update(resource_values(resource_file.read_text()))
            if mode != "git":
                after = metrics()
                sample_result["server_metrics_delta"] = {k: v - before_metrics.get(k, 0) for k, v in after.items()}
                assert all(v >= 0 for v in sample_result["server_metrics_delta"].values()), "server counters reset during trial"
                if before_server:
                    last = process_values(a.server_pid)
                    sample_result["server_cpu_seconds"] = last["cpu_seconds"] - before_server["cpu_seconds"]
                    sample_result["server_sampled_peak_rss_bytes"] = max(*rss, last["rss_bytes"])
        return sample_result, done.stdout

    def git(root, *args):
        return ["git", "-C", root, "-c", "diff.ignoreSubmodules=all", "-c",
                f"core.sparseCheckout={'true' if root in sparse_roots else 'false'}",
                "-c", "core.sparseCheckoutCone=false", *args]

    def scion(root, *args):
        return [binary, "-C", root, *args]

    sha = run(["git", "--git-dir", repo, "rev-parse", "HEAD"])[1].decode().strip()
    result["snapshot"] = sha
    expected_raw = run(["git", "--git-dir", repo, "ls-tree", "-rz", sha])[1]
    expected = {}
    for entry in filter(None, expected_raw.split(b"\0")):
        info, path = entry.split(b"\t", 1)
        mode, kind, oid = info.split()
        if kind == b"blob":
            expected[os.fsdecode(path)] = (mode, oid)
    result["files"] = len(expected)
    assert expected, "empty repository"
    for mode in modes:
        if mode != "git":
            expected_id = json.loads(run([binary, "--json", "show", f"git:{sha}"], mode)[1])["data"]["id"]
            main_id = json.loads(run([binary, "--json", "show", "main"], mode)[1])["data"]["id"]
            assert expected_id == main_id, "Scion main does not match the pinned Git commit"
            result["validation"][f"{mode}-changeset"] = expected_id

    def checkout(mode, root, sparse):
        disposable.append(root)
        if mode != "git":
            args = [binary, "checkout", "main", root]
            if sparse:
                args += ["--sparse", scope]
            return run(args, mode, True)[0]
        git_worktrees.append(root)
        if not sparse:
            return run(["git", "--git-dir", repo, "worktree", "add", "--detach", root, sha], mode, True)[0]
        run(["git", "--git-dir", repo, "worktree", "add", "--detach", "--no-checkout", root, sha])
        sparse_roots.add(root)
        location = Path(run(git(root, "rev-parse", "--git-path", "info/sparse-checkout"))[1].decode().strip())
        if not location.is_absolute():
            location = root / location
        location.parent.mkdir(parents=True, exist_ok=True)
        location.write_text(f"/{scope}/\n")
        # Sparse Git configuration is prepared outside the timer; materialization is timed.
        return run(git(root, "reset", "--hard", sha), mode, True)[0]

    def check_paths(root, sparse):
        actual = {str(f.relative_to(root)) for f in root.rglob("*") if not ({".git", ".scion"} & set(f.relative_to(root).parts)) and (f.is_file() or f.is_symlink())}
        want = {path for path in expected if not sparse or path.startswith(scope + "/")}
        assert actual == want, (root, list(want - actual)[:5], list(actual - want)[:5])
        for name in want:
            path = root / name
            mode, oid = expected[name]
            assert path.is_symlink() == (mode == b"120000"), name
            content = os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()
            encoded = b"blob " + str(len(content)).encode() + b"\0" + content
            assert hashlib.sha1(encoded).hexdigest().encode() == oid, name
            if not path.is_symlink():
                assert bool(path.stat().st_mode & 0o100) == (mode == b"100755"), name
        return len(actual)

    def measure(name, commands):
        for _ in range(2):
            for mode, cmd in commands.items():
                run(cmd, mode)
        values = {mode: [] for mode in commands}
        for _ in range(a.command_runs):
            order = list(commands)
            random.shuffle(order)
            for mode in order:
                values[mode].append(run(commands[mode], mode, True)[0])
        result["samples"][name] = values
        save()
        print(name, {m: round(statistics.median(v["seconds"] for v in vs), 4) for m, vs in values.items()}, flush=True)

    try:
        roots = {}
        for sparse in [False, True]:
            name = "sparse checkout" if sparse else "full checkout"
            participants = ["git", "daemon"] if a.extended_only and not sparse and "daemon" in modes else list(modes)
            if a.extended_only and not sparse and "daemon" not in modes:
                continue
            values = {mode: [] for mode in participants}
            for trial in range(a.checkout_runs + 1):
                order = list(participants)
                random.shuffle(order)
                for mode in order:
                    root = work / f"{mode}-{'sparse' if sparse else 'full'}-{trial}"
                    sample_result = checkout(mode, root, sparse)
                    if trial == 0:
                        result["first_checkout"].setdefault(name, {})[mode] = sample_result
                    else:
                        values[mode].append(sample_result)
                    count = check_paths(root, sparse)
                    roots[(mode, sparse)] = root
                    result["validation"][f"{mode}-{name}-files"] = count
                    save()
                    print(name, trial, mode, round(sample_result["seconds"], 3), "seconds", count, "files", flush=True)
            result["samples"][name] = values
            save()
        for sparse in [False, True]:
            active = {mode: root for (mode, is_sparse), root in roots.items() if is_sparse == sparse}
            label = "sparse " if sparse else ""
            status_commands = {mode: git(root, "status", "--porcelain=v1", "--untracked-files=all") if mode == "git" else scion(root, "status") for mode, root in active.items()}
            for mode, root in active.items():
                if mode == "git":
                    assert not run(status_commands[mode], mode)[1]
                else:
                    assert json.loads(run(scion(root, "--json", "status"), mode)[1])["data"]["clean"]
            measure(label + "clean status", status_commands)
            if not sparse:
                measure("history (100 first-parent commits)", {mode: git(root, "log", "--first-parent", "-n", "100", "--format=%h %s", sha) if mode == "git" else scion(root, "log", "--rev", "main", "--first-parent", "--limit", "100", "--oneline") for mode, root in active.items()})
                want = run(["git", "--git-dir", repo, "show", f"{sha}:README.md"])[1]
                read_commands = {mode: git(root, "show", f"{sha}:README.md") if mode == "git" else scion(root, "cat", "main:README.md") for mode, root in active.items()}
                for mode, cmd in read_commands.items():
                    assert run(cmd, mode)[1] == want
                measure("read README.md", read_commands)
            edited = next(path for path, (mode, _) in expected.items() if (not sparse or path.startswith(scope + "/")) and mode == b"100644")
            originals = {root: (root / edited).read_bytes() for root in active.values()}
            try:
                for root, original in originals.items():
                    (root / edited).write_bytes(original + b"\nTemporary benchmark edit.\n")
                for mode, root in active.items():
                    if mode == "git":
                        assert edited.encode() in run(status_commands[mode], mode)[1]
                    else:
                        data = json.loads(run(scion(root, "--json", "status"), mode)[1])["data"]
                        assert not data["clean"] and len(data["modified"]) == 1
                measure(label + "status (one edited file)", status_commands)
            finally:
                for root, original in originals.items():
                    (root / edited).write_bytes(original)
        if a.importer:
            clone_samples = []
            commit_count = int(run(["git", "--git-dir", repo, "rev-list", "--count", sha])[1])
            for trial in range(a.clone_runs):
                clone = work / f"git-clone-{trial}"
                disposable.append(clone)
                sample_result, _ = run([
                    "git", "clone", "--no-local", "--single-branch", "--branch", "main",
                    "--no-tags", "--no-checkout", repo.as_uri(), clone,
                ], "git", True)
                assert run(git(clone, "rev-parse", "HEAD"))[1].decode().strip() == sha
                assert int(run(git(clone, "rev-list", "--count", "HEAD"))[1]) == commit_count
                clone_samples.append(sample_result)
                print("local full-history clone", trial, round(sample_result["seconds"], 3), flush=True)
            result["samples"]["local full-history clone (no checkout)"] = {"git": clone_samples}
            commands = {"git": git(clone, "fetch", "origin")}
            for mode in modes:
                if mode != "git":
                    commands[mode] = [a.importer.resolve(), "--git", repo, "--ref", "HEAD", "--target-ref", "main", "--skip-submodules"]
                    _, out = run(commands[mode], mode)
                    report = json.loads(out)
                    assert report["imported"] == 0 and report["blobs_uploaded"] == 0
            measure("no-change sync", commands)
        result["summary"] = {name: {mode: {"median_seconds": statistics.median(s["seconds"] for s in samples), "range_seconds": [min(s["seconds"] for s in samples), max(s["seconds"] for s in samples)]} for mode, samples in by_mode.items()} for name, by_mode in result["samples"].items()}
        save()
        print(json.dumps(result["summary"], indent=2), flush=True)
    finally:
        for root in git_worktrees:
            if root.exists():
                run(["git", "--git-dir", repo, "worktree", "remove", "--force", root])
        for root in disposable:
            if root.exists():
                shutil.rmtree(root)


if __name__ == "__main__":
    main()
