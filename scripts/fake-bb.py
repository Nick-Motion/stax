#!/usr/bin/env python3
"""Fake `bb` CLI for sync benchmarks (point BB_CLI at this file).

Serves a recorded-shape registry from $FAKE_BB_DIR/registry.json (written by
motion-net-perf-lab.py), logs every call to $FAKE_BB_LOG as JSONL, and sleeps
so each call takes the measured real latency of that subcommand when
FAKE_BB_LATENCY=real (0 = process overhead only). Mutating subcommands only
touch the snapshot: `environment delete` models BB teardown by running
`git worktree remove --force` on the registered path and marking the
environment destroyed in $FAKE_BB_DIR/state.json.
"""

import json
import os
import subprocess
import sys
import time

STARTED = time.monotonic()
WALL_STARTED = time.time()

# Median wall seconds of the real bb CLI on this host (bench/bb/latency.tsv,
# live/bb-cli-timing.tsv), 2026-10-01.
REAL_LATENCY = {
    "status": 0.89,
    "environment list": 0.30,
    "environment providers": 0.28,
    "environment show": 0.21,
    "environment delete": 0.30,
    "project show": 0.21,
    "thread list": 0.25,
    "thread show": 0.21,
    "thread archive": 0.25,
}


def kind_of(argv):
    return " ".join(argv[:2]) if argv and argv[0] in ("environment", "project", "thread") else (argv[0] if argv else "")


def load_state(path):
    try:
        with open(path) as fh:
            return json.load(fh)
    except FileNotFoundError:
        return {"destroyed": [], "archived": []}


def respond(argv, registry, state, state_path):
    kind = kind_of(argv)
    envs = {e["id"]: e for e in registry["environments"]}
    if kind == "status":
        return registry["status"]
    if kind == "environment list":
        offset = int(argv[argv.index("--offset") + 1])
        limit = int(argv[argv.index("--limit") + 1])
        live = [e for e in registry["environments"] if e["id"] not in state["destroyed"]]
        return live[offset: offset + limit]
    if kind == "environment providers":
        return registry["providers"]
    if kind == "project show":
        return registry["project"]
    if kind == "environment show":
        env = dict(envs[argv[2]])
        if env["id"] in state["destroyed"]:
            env["status"] = "destroyed"
        return env
    if kind == "thread list":
        env_id = argv[argv.index("--environment") + 1]
        return [t for t in registry["threads"].get(env_id, []) if t["id"] not in state["archived"]]
    if kind == "thread show":
        for threads in registry["threads"].values():
            for t in threads:
                if t["id"] == argv[2]:
                    return {"thread": t}
        raise KeyError(argv[2])
    if kind == "thread archive":
        state["archived"].append(argv[2])
        save(state_path, state)
        return {"ok": True}
    if kind == "environment delete":
        env = envs[argv[2]]
        subprocess.run(["git", "worktree", "remove", "--force", env["path"]], cwd=registry["main_repo"],
                       check=True, capture_output=True)
        state["destroyed"].append(env["id"])
        save(state_path, state)
        return {"ok": True}
    raise SystemExit(f"fake bb: unsupported command {argv}")


def save(path, state):
    tmp = path + ".tmp"
    with open(tmp, "w") as fh:
        json.dump(state, fh)
    os.replace(tmp, path)


def main():
    argv = sys.argv[1:]
    root = os.environ["FAKE_BB_DIR"]
    with open(os.path.join(root, "registry.json")) as fh:
        registry = json.load(fh)
    state_path = os.path.join(root, "state.json")
    state = load_state(state_path)
    rc, out = 0, None
    try:
        out = respond(argv, registry, state, state_path)
    except Exception as error:  # surfaced to stax as a failed bb call
        rc = 1
        sys.stderr.write(f"fake bb error: {error}\n")
    if os.environ.get("FAKE_BB_LATENCY", "0") == "real":
        remaining = REAL_LATENCY.get(kind_of(argv), 0.25) - (time.monotonic() - STARTED)
        if remaining > 0:
            time.sleep(remaining)
    log = os.environ.get("FAKE_BB_LOG")
    if log:
        with open(log, "a") as fh:
            fh.write(json.dumps({"start": WALL_STARTED, "secs": round(time.monotonic() - STARTED, 4),
                                 "kind": kind_of(argv), "argv": argv, "rc": rc}) + "\n")
    if out is not None:
        sys.stdout.write(json.dumps(out))
    return rc


if __name__ == "__main__":
    sys.exit(main())
