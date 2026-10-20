#!/usr/bin/env python3
"""Matched real-sync benchmark for the sync history cache."""

import argparse
import hashlib
import json
from collections import Counter
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import time

BASE = "bd6c009371c24bf7e51456ea62bb5ba3282f1094"
REPO = Path(__file__).resolve().parents[1]


def run(args, *, cwd=None, env=None, stdout=subprocess.PIPE):
    result = subprocess.run(args, cwd=cwd, env=env, stdout=stdout, stderr=subprocess.PIPE)
    if result.returncode:
        raise RuntimeError(f"{args!r} failed: {result.stderr.decode(errors='replace')}")
    return result.stdout or b""


def snapshot(repo, env):
    return {
        "branches": run(["git", "for-each-ref", "--format=%(refname:short) %(objectname)", "refs/heads"], cwd=repo, env=env).decode().splitlines(),
        "metadata": run(["git", "for-each-ref", "--format=%(refname) %(objectname)", "refs/branch-metadata"], cwd=repo, env=env).decode().splitlines(),
    }


def fixture(repo, bare, home, binary, wrappers, git_log, workload):
    repo.mkdir()
    bare.mkdir()
    home.mkdir()
    env = os.environ.copy()
    env.update({
        "PATH": f"{wrappers}:{env['PATH']}",
        "STAX_GIT_LOG": str(git_log),
        "STAX_CONFIG_DIR": str(home / "stax"),
        "GH_CONFIG_DIR": str(home / "gh"),
        "GIT_AUTHOR_NAME": "Benchmark User",
        "GIT_AUTHOR_EMAIL": "benchmark@example.invalid",
        "GIT_COMMITTER_NAME": "Benchmark User",
        "GIT_COMMITTER_EMAIL": "benchmark@example.invalid",
        "GIT_AUTHOR_DATE": "2020-01-01T00:00:00Z",
        "GIT_COMMITTER_DATE": "2020-01-01T00:00:00Z",
        "GIT_CONFIG_NOSYSTEM": "1",
    })
    run(["git", "init", "-b", "main", str(repo)], env=env)
    run(["git", "init", "--bare", str(bare)], env=env)
    run(["git", "config", "user.name", "Benchmark User"], cwd=repo, env=env)
    run(["git", "config", "user.email", "benchmark@example.invalid"], cwd=repo, env=env)
    (repo / "README.md").write_text("fixed benchmark base\n")
    run(["git", "add", "README.md"], cwd=repo, env=env)
    run(["git", "commit", "-m", "base"], cwd=repo, env=env)
    run(["git", "remote", "add", "origin", str(bare)], cwd=repo, env=env)
    run(["git", "push", "-u", "origin", "main"], cwd=repo, env=env)
    if workload == "ambiguous-history":
        run([str(binary), "bc", "shared"], cwd=repo, env=env)
        (repo / "shared.txt").write_text("same patch in trunk and two branches\n")
        run(["git", "add", "shared.txt"], cwd=repo, env=env)
        run(["git", "commit", "-m", "shared patch"], cwd=repo, env=env)
        shared = run(["git", "rev-parse", "HEAD"], cwd=repo, env=env).decode().strip()
        run(["git", "switch", "main"], cwd=repo, env=env)
        run([str(binary), "bc", "topic-a"], cwd=repo, env=env)
        (repo / "pre-a.txt").write_text("branch a diverges before shared patch\n")
        run(["git", "add", "pre-a.txt"], cwd=repo, env=env)
        run(["git", "commit", "-m", "topic a divergence"] , cwd=repo, env=env)
        run(["git", "cherry-pick", shared], cwd=repo, env=env)
        (repo / "a.txt").write_text("unique a\n")
        run(["git", "add", "a.txt"], cwd=repo, env=env)
        run(["git", "commit", "-m", "topic a"], cwd=repo, env=env)
        run(["git", "switch", "main"], cwd=repo, env=env)
        run([str(binary), "bc", "topic-b"], cwd=repo, env=env)
        (repo / "pre-b.txt").write_text("branch b diverges before shared patch\n")
        run(["git", "add", "pre-b.txt"], cwd=repo, env=env)
        run(["git", "commit", "-m", "topic b divergence"], cwd=repo, env=env)
        run(["git", "cherry-pick", shared], cwd=repo, env=env)
        (repo / "b.txt").write_text("unique b\n")
        run(["git", "add", "b.txt"], cwd=repo, env=env)
        run(["git", "commit", "-m", "topic b"], cwd=repo, env=env)
        run(["git", "switch", "main"], cwd=repo, env=env)
        run(["git", "cherry-pick", shared], cwd=repo, env=env)
        (repo / "trunk-only.txt").write_text("unrelated trunk advance\n")
        run(["git", "add", "trunk-only.txt"], cwd=repo, env=env)
        run(["git", "commit", "-m", "advance trunk beyond common history"], cwd=repo, env=env)
        run(["git", "branch", "-D", "shared"], cwd=repo, env=env)
        run(["git", "update-ref", "-d", "refs/branch-metadata/shared"], cwd=repo, env=env)
        run(["git", "push", "origin", "main"], cwd=repo, env=env)
    return env


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--base", default=BASE)
    args = parser.parse_args()
    cargo = shutil.which("cargo") or str(Path.home() / ".cargo/bin/cargo")
    git = shutil.which("git")
    if not git:
        raise RuntimeError("git is unavailable")

    scratch = Path(tempfile.mkdtemp(prefix="stax-sync-history-bench-"))
    build_root = Path(tempfile.gettempdir()) / "stax-sync-history-cache-build"
    build_root.mkdir(exist_ok=True)
    try:
        base = scratch / "base-source"
        base.mkdir()
        archive = subprocess.Popen([git, "archive", args.base], cwd=REPO, stdout=subprocess.PIPE)
        with tarfile.open(fileobj=archive.stdout, mode="r|*") as tar:
            tar.extractall(base)
        if archive.wait():
            raise RuntimeError("failed to extract base source")
        candidate_target = build_root / "candidate-target"
        baseline_target = build_root / "baseline-target"
        run([cargo, "build", "--release", "--locked", "--bin", "stax"], cwd=REPO,
            env={**os.environ, "CARGO_TARGET_DIR": str(candidate_target)})
        run([cargo, "build", "--release", "--locked", "--bin", "stax"], cwd=base,
            env={**os.environ, "CARGO_TARGET_DIR": str(baseline_target)})
        binaries = {
            "baseline": baseline_target / "release/stax",
            "candidate": candidate_target / "release/stax",
        }
        wrappers = scratch / "bin"
        wrappers.mkdir()
        wrapper = wrappers / "git"
        wrapper.write_text(f"#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$STAX_GIT_LOG\"\nexec {git!s} \"$@\"\n")
        wrapper.chmod(0o755)
        results = []
        decisions = {}
        for workload in ("no-op", "ambiguous-history"):
            for repeat in range(args.repeats):
                for label in ("baseline", "candidate"):
                    run_root = scratch / f"{workload}-{repeat}-{label}"
                    run_root.mkdir()
                    repo, bare, home = run_root / "repo", run_root / "remote.git", run_root / "home"
                    log = run_root / "git.log"
                    env = fixture(repo, bare, home, binaries[label], wrappers, log, workload)
                    before = snapshot(repo, env)
                    if workload == "ambiguous-history":
                        assert any(line.startswith("topic-a ") for line in before["branches"]), before
                        assert any(line.startswith("topic-b ") for line in before["branches"]), before
                    log.write_text("")
                    started = time.perf_counter_ns()
                    sync_output = run([str(binaries[label]), "sync", "--force"], cwd=repo, env=env)
                    if workload == "ambiguous-history" and b"not deleting" not in sync_output:
                        raise RuntimeError(f"fixture did not reach partial-history retention: {sync_output.decode(errors='replace')}")
                    elapsed = time.perf_counter_ns() - started
                    trace = log.read_text().splitlines()
                    scans = [line for line in trace if line.startswith("log --format=%H -p --no-merges ")]
                    scan_counts = Counter(scans)
                    after = snapshot(repo, env)
                    if workload == "ambiguous-history":
                        assert any(line.startswith("topic-a ") for line in after["branches"]), after
                        assert any(line.startswith("topic-b ") for line in after["branches"]), after
                    decisions[(workload, repeat, label)] = {"before": before, "after": after}
                    results.append({"workload": workload, "repeat": repeat + 1, "variant": label,
                                    "elapsed_ms": elapsed / 1_000_000, "git_subprocesses": len(trace),
                                    "patch_history_scans": len(scans),
                                    "duplicate_patch_history_scans": sum(n - 1 for n in scan_counts.values()),
                                    "patch_history_scan_ranges": scans,
                                    "forge_requests": 0, "sync_succeeded": True,
                                    "sync_output_bytes": len(sync_output)})
        equivalence = []
        for workload in ("no-op", "ambiguous-history"):
            for repeat in range(args.repeats):
                baseline = decisions[(workload, repeat, "baseline")]
                candidate = decisions[(workload, repeat, "candidate")]
                equal = baseline == candidate
                equivalence.append({"workload": workload, "repeat": repeat + 1, "same_before_after_refs": equal})
                if not equal:
                    raise RuntimeError(f"baseline/candidate decisions differ: {workload} repeat {repeat + 1}")
        medians = {}
        for workload in ("no-op", "ambiguous-history"):
            medians[workload] = {}
            for variant in ("baseline", "candidate"):
                values = [row["elapsed_ms"] for row in results if row["workload"] == workload and row["variant"] == variant]
                medians[workload][variant] = sorted(values)[len(values) // 2]
        tracked_diff = run([git, "diff", "--binary", "HEAD"], cwd=REPO)
        untracked = {}
        for relative in ("src/commands/sync_retirement.rs", "src/engine/sync_lifecycle.rs", "scripts/benchmark-sync-history-cache.py"):
            path = REPO / relative
            if path.exists():
                untracked[relative] = hashlib.sha256(path.read_bytes()).hexdigest()
        print(json.dumps({"base": args.base, "candidate_head": run([git, "rev-parse", "HEAD"], cwd=REPO).decode().strip(),
                          "candidate_tracked_diff_sha256": hashlib.sha256(tracked_diff).hexdigest(),
                          "candidate_untracked_sha256": untracked,
                          "build_locations": {"baseline_target": str(baseline_target), "candidate_target": str(candidate_target),
                                              "baseline_checkout": str(base)},
                          "commands": ["cargo build --release --locked --bin stax", "stax sync --force"],
                          "versions": {**{key: run([str(path), "--version"]).decode().strip() for key, path in binaries.items()},
                                       "git": run([git, "--version"]).decode().strip(),
                                       "rustc": run([shutil.which("rustc") or str(Path.home() / ".cargo/bin/rustc"), "--version"]).decode().strip()},
                          "repeats": args.repeats, "results": results, "medians_ms": medians,
                          "equivalence": equivalence}, indent=2))
    finally:
        shutil.rmtree(scratch)


if __name__ == "__main__":
    main()
