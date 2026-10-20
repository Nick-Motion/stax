#!/usr/bin/env python3
"""Interleaved wall-time benchmark of stax binaries against a live repository.

Every repeat runs each (binary, command) pair once, in order, so warmup, the
page cache and host noise land on every side alike. Each run records wall
seconds, the `--trace` git summary (count and milliseconds inside git), the
exit code and a digest of the non-trace output, so a faster side that did
different work or failed shows up in the report instead of as a speedup.
Failed runs are counted but excluded from the timing summary, and the exit
code is nonzero when any run failed. A command that mutates the repository
(plain `sync`) changes the state later runs see, so compare with `--dry-run`
or reset the repository between repeats.

Example:
  benchmark-live.py --repo ~/src/app \
      --binary upstream=/usr/local/bin/stax --binary fork=target/release/stax \
      --command "sync --dry-run" --command "sync" --repeats 7 --out bench.json
"""

import argparse
import hashlib
import json
import os
import re
import shlex
import statistics
import subprocess
import sys
import time

SUMMARY = re.compile(r"\[trace\] (\d+) git commands in ([\d.]+)ms; command wall time ([\d.]+)ms")


def run_once(binary, command, repo):
    env = dict(os.environ, STAX_DISABLE_UPDATE_CHECK="1", NO_COLOR="1")
    argv = [binary, *shlex.split(command), "--trace"]
    start = time.perf_counter()
    proc = subprocess.run(argv, cwd=repo, env=env, capture_output=True, text=True)
    wall = time.perf_counter() - start
    text = proc.stdout + proc.stderr
    match = SUMMARY.search(text)
    visible = "\n".join(line for line in text.splitlines() if not line.startswith("[trace]"))
    return {
        "wall_s": round(wall, 3),
        "exit": proc.returncode,
        "git_calls": int(match.group(1)) if match else None,
        "git_ms": float(match.group(2)) if match else None,
        "output_sha": hashlib.sha256(visible.encode()).hexdigest()[:12],
    }


def summarize(runs):
    rows = {}
    for run in runs:
        rows.setdefault((run["binary"], run["command"]), []).append(run)
    out = []
    for (binary, command), group in rows.items():
        ok = [r for r in group if r["exit"] == 0]
        walls = [r["wall_s"] for r in ok]
        git_ms = [r["git_ms"] for r in ok if r["git_ms"] is not None]
        out.append({
            "binary": binary,
            "command": command,
            "runs": len(group),
            "median_s": round(statistics.median(walls), 3) if walls else None,
            "min_s": min(walls) if walls else None,
            "max_s": max(walls) if walls else None,
            "median_git_ms": statistics.median(git_ms) if git_ms else None,
            "git_calls": sorted({r["git_calls"] for r in ok if r["git_calls"] is not None}),
            "failures": len(group) - len(ok),
            "outputs": sorted({r["output_sha"] for r in group}),
        })
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--binary", action="append", required=True, help="NAME=PATH")
    ap.add_argument("--command", action="append", required=True, help="stax arguments, quoted")
    ap.add_argument("--repeats", type=int, default=5)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    if args.repeats < 1:
        ap.error("--repeats must be at least 1")
    binaries = dict(spec.split("=", 1) for spec in args.binary)

    runs = []
    for repeat in range(args.repeats):
        for command in args.command:
            for name, path in binaries.items():
                run = run_once(path, command, args.repo)
                run.update(repeat=repeat, binary=name, command=command)
                runs.append(run)
                print(f"{repeat} {name:>10} {command:<16} {run['wall_s']:7.3f}s git={run['git_ms']} exit={run['exit']}", file=sys.stderr)

    report = {"repo": args.repo, "load_average": os.getloadavg(), "summary": summarize(runs), "runs": runs}
    with open(args.out, "w") as fh:
        json.dump(report, fh, indent=1)
    for row in report["summary"]:
        timing = (f"median {row['median_s']:7.3f}s range {row['min_s']:.3f}-{row['max_s']:.3f}s"
                  if row["median_s"] is not None else "no successful run")
        print(f"{row['binary']:>10} {row['command']:<16} {timing} git {row['median_git_ms']}ms "
              f"calls {row['git_calls']} failures {row['failures']} outputs {len(row['outputs'])}")
    sys.exit(1 if any(r["exit"] != 0 for r in runs) else 0)


if __name__ == "__main__":
    main()
