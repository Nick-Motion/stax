#!/usr/bin/env python3
"""Build-free `stax sync` benchmark against motion.net-derived perf-lab snapshots.

For every run: reset the snapshot to its golden state (untimed, fingerprint
verified), point HOME at the snapshot's isolated home (stax config -> local
replay forge), run the given binary, and record wall time, Git subprocesses
by subcommand (GIT_TRACE2_EVENT, top-level processes only), forge requests
(replay-server log), BB CLI calls (fake bb log), exit code and the post-run
outcome fingerprint. Variant order is shuffled per repeat (seeded).

Example:
  benchmark-sync-motion-net.py --binary candidate=/path/to/stax \
      --case noop-10 --mode sync --profile zero --profile real --repeats 7 \
      --out PERF/bench --tag baseline

Profiles: zero = 0 ms forge latency + fake bb process overhead only;
real = 420 ms per forge GET (measured Gitea median) + measured bb latencies.
"""

import argparse
import importlib.util
import json
import os
import random
import re
import statistics
import subprocess
import sys
import time
import urllib.request
import zlib
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("lab", SCRIPTS / "motion-net-perf-lab.py")
lab = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lab)

PROFILES = {"zero": {"forge_ms": 0, "bb": "0"}, "real": {"forge_ms": 420, "bb": "real"}}
LOGS = lab.LOGS_DIR / "runs"


# ---------------------------------------------------------------- pure core

def parse_trace2(lines):
    """Aggregate top-level git processes (sid without '/') by subcommand."""
    procs = {}
    for line in lines:
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        sid = ev.get("sid", "")
        p = procs.setdefault(sid, {"name": None, "secs": 0.0, "argv": None})
        if ev.get("event") == "start":
            p["argv"] = ev.get("argv")
        elif ev.get("event") == "cmd_name":
            p["name"] = ev.get("name")
        elif ev.get("event") in ("exit", "atexit"):
            p["secs"] = max(p["secs"], float(ev.get("t_abs", 0.0)))
    top, nested = {}, 0
    for sid, p in procs.items():
        if "/" in sid:
            nested += 1
            continue
        name = p["name"] or (p["argv"][1] if p["argv"] and len(p["argv"]) > 1 else "?")
        agg = top.setdefault(name, {"count": 0, "secs": 0.0})
        agg["count"] += 1
        agg["secs"] += p["secs"]
    count = sum(v["count"] for v in top.values())
    secs = sum(v["secs"] for v in top.values())
    return {"count": count, "secs": round(secs, 4), "nested": nested,
            "by_cmd": {k: {"count": v["count"], "secs": round(v["secs"], 4)} for k, v in sorted(top.items())}}


PHASE = re.compile(r"^\s*[\u2713\u26a0\u25b8\u2717 ]*\s*([A-Za-z][^|]*?)\s{2,}([0-9]+\.[0-9]+)s\s*$")
TOTAL = re.compile(r"\|\s*([0-9]+\.[0-9]+)s\s*$")


def parse_phases(text):
    """stax's own per-step timers (e.g. '  ✓ Fetch origin   0.105s')."""
    phases, total = {}, None
    for line in text.splitlines():
        m = PHASE.match(line)
        if m:
            phases[m.group(1).strip()] = phases.get(m.group(1).strip(), 0.0) + float(m.group(2))
        elif line.startswith("Sync complete") and TOTAL.search(line):
            total = float(TOTAL.search(line).group(1))
    return {"steps": phases, "reported_total": total}


def summarize_requests(rows, key):
    by = {}
    for r in rows:
        agg = by.setdefault(r[key], {"count": 0, "secs": 0.0})
        agg["count"] += 1
        agg["secs"] += r.get("secs", r.get("end", 0) - r.get("start", 0))
    span = (max(r.get("end", r["start"] + r.get("secs", 0)) for r in rows) - min(r["start"] for r in rows)) if rows else 0.0
    return {"count": len(rows), "secs": round(sum(v["secs"] for v in by.values()), 4), "span": round(span, 4),
            "by": {k: {"count": v["count"], "secs": round(v["secs"], 4)} for k, v in sorted(by.items())}}


def quantiles(values):
    values = sorted(values)
    if not values:
        return {}
    q = statistics.quantiles(values, n=4) if len(values) > 1 else [values[0]] * 3
    return {"n": len(values), "median": round(statistics.median(values), 4), "p25": round(q[0], 4),
            "p75": round(q[2], 4), "min": round(values[0], 4), "max": round(values[-1], 4)}


def summarize(runs):
    groups = {}
    for r in runs:
        if r.get("warmup"):
            continue
        groups.setdefault((r["case"], r["mode"], r["profile"], r["variant"]), []).append(r)
    out = []
    for (case, mode, profile, variant), rs in sorted(groups.items()):
        cmds = sorted({c for r in rs for c in r["git"]["by_cmd"]})
        out.append({
            "case": case, "mode": mode, "profile": profile, "variant": variant,
            "wall_secs": quantiles([r["wall_secs"] for r in rs]),
            "git_count": quantiles([r["git"]["count"] for r in rs]),
            "git_secs": quantiles([r["git"]["secs"] for r in rs]),
            "git_by_cmd_median": {c: {"count": statistics.median([r["git"]["by_cmd"].get(c, {"count": 0})["count"] for r in rs]),
                                      "secs": round(statistics.median([r["git"]["by_cmd"].get(c, {"secs": 0})["secs"] for r in rs]), 4)}
                                  for c in cmds},
            "forge_count": quantiles([r["forge"]["count"] for r in rs]),
            "forge_secs_sum": quantiles([r["forge"]["secs"] for r in rs]),
            "forge_span": quantiles([r["forge"]["span"] for r in rs]),
            "forge_by": rs[0]["forge"]["by"],
            "bb_count": quantiles([r["bb"]["count"] for r in rs]),
            "bb_secs": quantiles([r["bb"]["secs"] for r in rs]),
            "bb_by_median": {k: statistics.median([r["bb"]["by"].get(k, {"count": 0})["count"] for r in rs])
                             for k in sorted({k for r in rs for k in r["bb"]["by"]})},
            "phases_median": {k: round(statistics.median([r["phases"]["steps"].get(k, 0.0) for r in rs]), 4)
                              for k in sorted({k for r in rs for k in r["phases"]["steps"]})},
            "exit_codes": sorted({r["exit"] for r in rs}),
            "outcome_digests": sorted({r["outcome"]["digest"] for r in rs}),
            "heads_after": rs[0]["outcome"]["parts"]["heads"].__len__(),
            "worktrees_after": rs[0]["outcome"]["parts"]["worktree_count"],
        })
    return out


# ---------------------------------------------------------- imperative shell

def start_forge(log):
    port_file = lab.LOGS_DIR / "forge.port"
    port_file.unlink(missing_ok=True)
    proc = subprocess.Popen([sys.executable, str(SCRIPTS / "forge-replay-server.py"), "--log", str(log),
                             "--port-file", str(port_file),
                             "--statuses", str(lab.FORGE / "statuses.json")], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(100):
        if port_file.exists() and port_file.read_text():
            return proc, int(port_file.read_text())
        time.sleep(0.05)
    raise SystemExit("forge replay server did not start")


def forge_load(port, cdir, latency_ms):
    req = urllib.request.Request(f"http://127.0.0.1:{port}/__control/load?case={cdir}&latency_ms={latency_ms}",
                                 method="POST", data=b"")
    urllib.request.urlopen(req, timeout=5).read()


def read_new(path, offset):
    if not path.exists():
        return [], offset
    with open(path) as fh:
        fh.seek(offset)
        data = fh.read()
        return [l for l in data.splitlines() if l.strip()], fh.tell()


def run_once(binary, variant, case, mode, profile, port, forge_log, forge_offset, run_id):
    cdir = lab.case_dir(case)
    lab.reset(cdir)  # untimed; raises unless the golden fingerprint is reproduced
    (cdir / "home" / ".config" / "stax" / "config.toml").write_text(
        lab.stax_config(f"http://127.0.0.1:{port}/api/v1"))
    prof = PROFILES[profile]
    forge_load(port, cdir, prof["forge_ms"])
    LOGS.mkdir(parents=True, exist_ok=True)
    trace = LOGS / f"{run_id}.trace2.jsonl"
    bblog = LOGS / f"{run_id}.bb.jsonl"
    out = LOGS / f"{run_id}.out.log"
    for p in (trace, bblog):
        p.unlink(missing_ok=True)
    env = {
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin",
        "HOME": str(cdir / "home"),
        "TERM": "dumb", "NO_COLOR": "1", "LANG": "en_US.UTF-8",
        "STAX_DISABLE_UPDATE_CHECK": "1",
        "STAX_GITEA_TOKEN": "replay-forge-not-a-secret",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_TRACE2_EVENT": str(trace),
        "BB_CLI": str(SCRIPTS / "fake-bb.py"),
        "FAKE_BB_DIR": str(cdir / "bb"),
        "FAKE_BB_LOG": str(bblog),
        "FAKE_BB_LATENCY": prof["bb"],
    }
    args = [binary, "sync", "--force"] if mode == "sync" else [binary, "sync", "--dry-run"]
    with open(out, "w") as fh:
        started = time.perf_counter()
        proc = subprocess.run(args, cwd=cdir / "repo", env=env, stdout=fh, stderr=subprocess.STDOUT,
                              stdin=subprocess.DEVNULL)
        wall = time.perf_counter() - started
    forge_rows, forge_offset = read_new(forge_log, forge_offset)
    forge_rows = [json.loads(l) for l in forge_rows]
    bb_rows = [json.loads(l) for l in bblog.read_text().splitlines()] if bblog.exists() else []
    trace_lines = trace.read_text().splitlines() if trace.exists() else []
    outcome = lab.fingerprint(cdir)
    return {
        "run_id": run_id, "case": case, "mode": mode, "profile": profile, "variant": variant,
        "binary": binary, "wall_secs": round(wall, 4), "exit": proc.returncode,
        "git": parse_trace2(trace_lines),
        "forge": summarize_requests(forge_rows, "endpoint"),
        "bb": summarize_requests(bb_rows, "kind"),
        "phases": parse_phases(out.read_text(errors="replace")),
        "outcome": outcome, "log": str(out),
    }, forge_offset


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", action="append", required=True, help="NAME=PATH")
    ap.add_argument("--case", action="append", required=True)
    ap.add_argument("--mode", action="append", choices=("sync", "dry-run"), required=True)
    ap.add_argument("--profile", action="append", choices=tuple(PROFILES), required=True)
    ap.add_argument("--repeats", type=int, default=7)
    ap.add_argument("--seed", type=int, default=20261001)
    ap.add_argument("--no-warmup", action="store_true")
    ap.add_argument("--out", required=True)
    ap.add_argument("--tag", required=True)
    args = ap.parse_args()

    variants = [tuple(b.split("=", 1)) for b in args.binary]
    out_dir = Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)
    raw_path = out_dir / f"{args.tag}-raw.jsonl"
    forge_log = lab.LOGS_DIR / f"forge-{args.tag}.jsonl"
    server, port = start_forge(forge_log)
    forge_offset = forge_log.stat().st_size if forge_log.exists() else 0
    runs = []
    try:
        with open(raw_path, "a") as raw:
            for case in args.case:
                for mode in args.mode:
                    for profile in args.profile:
                        plan = [] if args.no_warmup else [(-1, variants[0])]
                        for rep in range(args.repeats):
                            order = variants[:]
                            random.Random(args.seed + rep + zlib.crc32(f"{case}/{mode}/{profile}".encode())).shuffle(order)
                            plan += [(rep, v) for v in order]
                        for rep, (vname, vpath) in plan:
                            run_id = f"{args.tag}-{case}-{mode}-{profile}-{vname}-r{rep}"
                            rec, forge_offset = run_once(vpath, vname, case, mode, profile, port, forge_log,
                                                         forge_offset, run_id)
                            rec["repeat"], rec["warmup"] = rep, rep < 0
                            raw.write(json.dumps(rec) + "\n")
                            raw.flush()
                            runs.append(rec)
                            print(f"{run_id} wall={rec['wall_secs']:.3f}s exit={rec['exit']} git={rec['git']['count']} "
                                  f"forge={rec['forge']['count']} bb={rec['bb']['count']} out={rec['outcome']['digest'][:12]}",
                                  flush=True)
    finally:
        server.terminate()
    summary = {"tag": args.tag, "variants": dict(variants), "repeats": args.repeats, "seed": args.seed,
               "profiles": PROFILES, "groups": summarize(runs)}
    (out_dir / f"{args.tag}-summary.json").write_text(json.dumps(summary, indent=1))
    for case in args.case:
        lab.reset(lab.case_dir(case))


if __name__ == "__main__":
    main()
