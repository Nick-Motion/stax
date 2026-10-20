#!/usr/bin/env python3
"""Compare linkers on this crate's incremental dev builds.

Each linker gets its own target directory so switching never rebuilds the
dependency graph. After an untimed warm build, every repeat touches a source
file and times `cargo build` (the stax binary) and `cargo build --tests` (the
integration test binary) for every linker in turn, so host noise lands on each
side alike. The linker command rustc actually ran is recorded per variant from
`--print link-args`, so a variant that silently fell back to the default shows
up in the report.

Example:
  benchmark-linkers.py --repo . --linker default= \
      --linker mold=/path/to/mold --linker wild=/path/to/wild --repeats 3 --out linkers.json
"""

import argparse
import json
import os
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

TRIPLE = "X86_64_UNKNOWN_LINUX_GNU"
PHASES = {
    "bin": ("src/main.rs", ["build", "--bin", "stax"]),
    "tests": ("tests/all_tests.rs", ["build", "--tests"]),
}


def env_for(name, path, target_root):
    env = dict(os.environ, CARGO_TARGET_DIR=str(target_root / name))
    if path:
        env[f"CARGO_TARGET_{TRIPLE}_LINKER"] = "clang"
        env[f"CARGO_TARGET_{TRIPLE}_RUSTFLAGS"] = f"-C link-arg=--ld-path={path}"
    return env


def cargo(args, repo, env):
    start = time.perf_counter()
    proc = subprocess.run(["cargo", *args], cwd=repo, env=env, capture_output=True, text=True)
    return round(time.perf_counter() - start, 2), proc


def linker_evidence(repo, env):
    _, proc = cargo(["rustc", "--bin", "stax", "--", "--print", "link-args"], repo, env)
    line = next((l for l in proc.stdout.splitlines() if "-o" in l and ("ld" in l or "cc" in l)), "")
    hits = re.findall(r'(?:"?[^"\s]*(?:ld-path|fuse-ld|gcc-ld|self-contained|linker-flavor)[^"\s]*"?)', line)
    return {"link_args_sample": line[:200], "linker_hints": hits[:6]}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--linker", action="append", required=True, help="NAME=PATH, empty PATH for the toolchain default")
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--target-root", help="defaults to <repo>/target-linkbench")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    repo = Path(args.repo).resolve()
    target_root = Path(args.target_root) if args.target_root else repo / "target-linkbench"
    linkers = dict(spec.split("=", 1) for spec in args.linker)

    evidence, runs = {}, []
    for name, path in linkers.items():
        env = env_for(name, path, target_root)
        for phase, (_, build) in PHASES.items():
            secs, proc = cargo(build, repo, env)
            print(f"warm {name:>8} {phase:<6} {secs:8.2f}s exit={proc.returncode}", file=sys.stderr)
            if proc.returncode:
                sys.exit(f"{name} {phase} warm build failed:\n{proc.stderr[-2000:]}")
        evidence[name] = linker_evidence(repo, env)
        print(f"link {name:>8} {evidence[name]['linker_hints']}", file=sys.stderr)

    for repeat in range(args.repeats):
        for phase, (touch, build) in PHASES.items():
            for name, path in linkers.items():
                env = env_for(name, path, target_root)
                (repo / touch).touch()
                secs, proc = cargo(build, repo, env)
                runs.append({"repeat": repeat, "linker": name, "phase": phase, "wall_s": secs, "exit": proc.returncode})
                print(f"{repeat} {name:>8} {phase:<6} {secs:8.2f}s exit={proc.returncode}", file=sys.stderr)

    summary = []
    for name in linkers:
        for phase in PHASES:
            walls = [r["wall_s"] for r in runs if r["linker"] == name and r["phase"] == phase]
            summary.append({"linker": name, "phase": phase, "runs": len(walls), "median_s": statistics.median(walls),
                            "min_s": min(walls), "max_s": max(walls),
                            "failures": sum(r["exit"] != 0 for r in runs if r["linker"] == name and r["phase"] == phase)})
    report = {"repo": str(repo), "load_average": os.getloadavg(), "linkers": linkers, "evidence": evidence,
              "summary": summary, "runs": runs}
    Path(args.out).write_text(json.dumps(report, indent=1))
    for row in summary:
        print(f"{row['linker']:>8} {row['phase']:<6} median {row['median_s']:7.2f}s range {row['min_s']:.2f}-{row['max_s']:.2f}s failures {row['failures']}")


if __name__ == "__main__":
    main()
