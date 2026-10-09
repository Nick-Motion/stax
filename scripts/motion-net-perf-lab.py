#!/usr/bin/env python3
"""Isolated, resettable motion.net-derived snapshots for `stax sync` benchmarks.

The live repository is only ever read: refs via `for-each-ref`, objects via
`objects/info/alternates`. Every snapshot lives under the perf lab and talks to
a local bare `origin.git` (through `url.<file>.insteadOf`), a local forge
replay server (`forge-replay-server.py`) and a fake `bb` (`fake-bb.py`).

Subcommands:
  origin                 build LAB/origin.git from the recorded server ls-remote
  base                   build LAB/base/primary (one real checkout of main)
  build CASE [CASE...]   build snapshots (see CASES) and their golden state
  reset CASE             restore a snapshot to its golden state, verify fingerprint
  fingerprint CASE       print the outcome fingerprint JSON
  list                   print known cases

Functional core: `select_pools`, `case_spec`, `fingerprint_digest` are pure.
Imperative shell: everything that runs git or touches the filesystem.
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field, asdict
from pathlib import Path

LIVE = Path("/Users/nicholasmurphy/workspaces/motion.net")
LAB = Path("/Users/nicholasmurphy/workspaces/stax-perf-lab")
PERF = Path("/Users/nicholasmurphy/.bb-machines/nm1.getbb.app/thread-storage/thr_mnyavqsgig/perf")
FORGE = PERF / "bench" / "forge"
REMOTE_URL = "https://git.wonderly.info/wonderlydotcom/backend-net.git"
ORIGIN = LAB / "origin.git"
BASE = LAB / "base" / "primary"
# Overrides let a second benchmark run on separate snapshot copies and logs
# while another matrix uses the defaults.
SNAPSHOTS = Path(os.environ.get("STAX_PERF_SNAPSHOTS") or LAB / "snapshots")
LOGS_DIR = Path(os.environ.get("STAX_PERF_LOGS") or LAB / "logs")
SHELL_WORKTREES = 59  # live: 60 worktrees incl. primary
BB_REGISTRY_ROWS = 27  # live: 27 environments on the host
TRUNK = "main"

CASES = (
    "live-mirror",
    "noop-10", "noop-50", "noop-100",
    "hist-10", "hist-50", "hist-100",
    "cleanup-10", "cleanup-25",
)


# --------------------------------------------------------------------------
# Functional core
# --------------------------------------------------------------------------

@dataclass(frozen=True)
class Branch:
    name: str
    tip: str
    klass: str  # open | merged | ambiguous | live
    pr: dict | None = None  # recorded PR object served by the replay forge
    pr_info_state: str | None = None  # metadata prInfo.state written at build
    worktree: str | None = None  # None | clean | tracked | untracked
    bb_owned: bool = False
    parent: str = TRUNK


@dataclass
class Spec:
    name: str
    description: str
    trunk_local: str  # local main OID
    branches: list = field(default_factory=list)
    live_refs: bool = False  # copy every live ref verbatim (live-mirror)
    derivation: str = ""
    fidelity_gaps: list = field(default_factory=list)


def pr_branch(pr):
    """Local branch name for a PR. Gitea reports deleted-branch PR heads as
    refs/pull/N/head; the label keeps the original branch name."""
    ref = pr["head"]["ref"]
    if ref.startswith("refs/pull/"):
        return pr["head"].get("label") or f"pr-{pr['number']}"
    return ref


def select_pools(prs, server_heads, have):
    """Partition recorded PRs (base main, head object present) by class."""
    def usable(pr):
        return pr["base"]["ref"] == TRUNK and pr["head"]["sha"] in have

    recent = sorted((p for p in prs if usable(p)), key=lambda p: (p["updated_at"] or "", p["number"]), reverse=True)
    seen, pools = set(), {"open": [], "merged_gone": [], "merged_any": [], "closed_gone": []}
    for pr in recent:
        head = pr_branch(pr)
        if head in seen:
            continue
        seen.add(head)
        on_remote = server_heads.get(head)
        if pr["merged"]:
            pools["merged_any"].append(pr)
            if on_remote is None:
                pools["merged_gone"].append(pr)
        elif pr["state"] == "open" and on_remote == pr["head"]["sha"]:
            pools["open"].append(pr)
        elif pr["state"] == "closed" and on_remote is None:
            pools["closed_gone"].append(pr)
    return pools


def case_spec(name, pools, origin_main, live_main):
    kind, _, size = name.partition("-")
    if kind == "live":
        return Spec(
            name=name,
            description="Exact copy of every live ref (75 local branches, 9 tracked, live main 61 behind origin) "
                        "plus 59 registered linked worktrees; dry-run only (the run the live profile could not do).",
            trunk_local=live_main,
            live_refs=True,
            derivation="git for-each-ref of live motion.net copied verbatim; origin.git = recorded server ls-remote heads.",
            fidelity_gaps=["linked worktrees are registered shells (no checkout); the CLI path never status-checks them",
                           "primary checkout is clean; live primary has uncommitted user changes"],
        )
    n = int(size)
    spec = Spec(name=name, description="", trunk_local=origin_main)
    if kind == "noop":
        picks = pools["open"][:n]
        spec.description = f"{n} tracked branches with open PRs whose remote head equals the local tip; trunk current. Nothing to delete."
        spec.derivation = f"{len(picks)} most recently updated real open PRs (base main, head object present, server head == PR head)."
        spec.branches = [Branch(pr_branch(p), p["head"]["sha"], "open", pr=p, pr_info_state="OPEN") for p in picks]
    elif kind == "hist":
        n_merged, n_amb = n // 2, n // 4
        n_open = n - n_merged - n_amb
        merged = pools["merged_gone"][:n_merged]
        amb = pools["closed_gone"][:n_amb]
        filler = pools["open"][len(pools["open"]) - (n_amb - len(amb)) - n_open:]
        amb_fill, opened = filler[: n_amb - len(amb)], filler[n_amb - len(amb):]
        spec.description = (f"{n} tracked branches without PR linkage: {len(merged)} squash-merged (remote deleted), "
                            f"{n_amb} ambiguous (remote gone, not merged), {len(opened)} with live remote heads.")
        spec.derivation = (f"merged = real merged PR heads whose server branch is deleted ({len(merged)}); ambiguous = "
                           f"{len(amb)} real closed-unmerged PR heads with deleted server branch + {len(amb_fill)} real open PR "
                           f"heads renamed under perf-ambiguous/ (so no remote branch exists); open = real open PR heads.")
        if amb_fill:
            spec.fidelity_gaps.append(f"{len(amb_fill)} ambiguous branches are real open-PR heads under a synthetic name")
        spec.branches = (
            [Branch(pr_branch(p), p["head"]["sha"], "merged") for p in merged]
            + [Branch(pr_branch(p), p["head"]["sha"], "ambiguous") for p in amb]
            + [Branch("perf-ambiguous/" + pr_branch(p), p["head"]["sha"], "ambiguous") for p in amb_fill]
            + [Branch(pr_branch(p), p["head"]["sha"], "open") for p in opened]
        )
    elif kind == "cleanup":
        picks = pools["merged_any"][:n]
        kinds = ("tracked", "tracked", "tracked", "untracked", "untracked", "clean", "clean", "clean", "clean", "clean")
        spec.description = (f"{n} PR-linked branches whose PR merged (exact head), each checked out in a linked worktree; "
                            f"30% tracked changes, 20% untracked-only, 50% clean; even-indexed are BB-owned via fake bb.")
        spec.derivation = f"{len(picks)} most recent real merged PRs (base main, head object present); metadata prInfo.state=OPEN (stale)."
        spec.fidelity_gaps += [
            "dirty contents are synthetic (2 modified files, 1 staged; or 3 small untracked files)",
            "fake bb registry: project gitRemoteUrl matches the snapshot remote (live BB project points at github.com and would keep every BB worktree as UnknownOwner)",
            "fake bb `environment delete` removes the worktree with `git worktree remove --force` (models BB teardown)",
        ]
        spec.branches = [
            Branch(pr_branch(p), p["head"]["sha"], "merged", pr=p, pr_info_state="OPEN",
                   worktree=kinds[i % len(kinds)], bb_owned=(i % 2 == 0))
            for i, p in enumerate(picks)
        ]
    else:
        raise SystemExit(f"unknown case {name}")
    if len(spec.branches) < n:
        spec.fidelity_gaps.append(f"only {len(spec.branches)} of {n} branches available from real data")
    return spec


def metadata_json(parent, parent_rev, pr_number=None, pr_state=None, is_draft=False):
    meta = {"parentBranchName": parent, "parentBranchRevision": parent_rev, "frozen": False}
    if pr_number is not None:
        meta["prInfo"] = {"number": pr_number, "state": pr_state, "isDraft": is_draft}
    return json.dumps(meta, separators=(",", ":"))


def fingerprint_digest(parts):
    canonical = json.dumps(parts, sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(canonical.encode()).hexdigest()


# --------------------------------------------------------------------------
# Imperative shell
# --------------------------------------------------------------------------

def git(cwd, *args, input=None, check=True, env=None):
    proc = subprocess.run(["git", *args], cwd=cwd, input=input, capture_output=True, text=True,
                          check=False, env=env)
    if check and proc.returncode != 0:
        raise SystemExit(f"git {' '.join(args)} in {cwd} failed: {proc.stderr.strip()}")
    return proc.stdout


def live_objects():
    return LIVE / ".git" / "objects"


def server_heads():
    heads = {}
    for line in (FORGE / "ls-remote.txt").read_text().splitlines():
        sha, ref = line.split()
        if ref.startswith("refs/heads/"):
            heads[ref[len("refs/heads/"):]] = sha
    return heads


def existing(cwd, shas):
    out = git(cwd, "cat-file", "--batch-check", input="\n".join(shas) + "\n")
    return {line.split()[0] for line in out.splitlines() if "missing" not in line}


def update_refs(cwd, desired, prefixes):
    """Make refs under `prefixes` exactly `desired` (ref -> oid)."""
    current = {}
    for line in git(cwd, "for-each-ref", "--format=%(refname) %(objectname)", *prefixes).splitlines():
        ref, oid = line.split()
        current[ref] = oid
    lines = [f"delete {ref}" for ref in current if ref not in desired]
    lines += [f"update {ref} {oid}" for ref, oid in desired.items() if current.get(ref) != oid]
    if lines:
        git(cwd, "update-ref", "--stdin", input="\n".join(lines) + "\n")


def cmd_origin(_args):
    if ORIGIN.exists():
        shutil.rmtree(ORIGIN)
    git(LAB, "init", "--bare", "-q", "-b", TRUNK, str(ORIGIN))
    (ORIGIN / "objects" / "info" / "alternates").write_text(f"{live_objects()}\n")
    refs = {}
    for line in (FORGE / "ls-remote.txt").read_text().splitlines():
        sha, ref = line.split()
        if ref.startswith(("refs/heads/", "refs/tags/")) and not ref.endswith("^{}"):
            refs[ref] = sha
    have = existing(ORIGIN, list(set(refs.values())))
    kept = {r: s for r, s in refs.items() if s in have}
    update_refs(ORIGIN, kept, ["refs/heads", "refs/tags"])
    git(ORIGIN, "symbolic-ref", "HEAD", f"refs/heads/{TRUNK}")
    report = {"server_refs": len(refs), "kept": len(kept), "missing_objects": len(refs) - len(kept)}
    (LAB / "origin.json").write_text(json.dumps(report, indent=1))
    print(json.dumps(report))


def configure_repo(repo):
    (repo / ".git" / "objects" / "info" / "alternates").write_text(f"{live_objects()}\n")
    for key, value in (
        ("core.hookspath", ".husky"),  # live setting; .husky has pre-commit/pre-push only
        ("core.ignorecase", "true"),
        ("core.precomposeunicode", "true"),
        ("remote.origin.url", REMOTE_URL),
        ("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"),
        (f"url.file://{ORIGIN}.insteadOf", REMOTE_URL),
        ("user.name", "Perf Lab"),
        ("user.email", "perf-lab@example.invalid"),
    ):
        git(repo, "config", "--local", key, value)


def remote_tracking(heads):
    return {f"refs/remotes/origin/{h}": s for h, s in heads.items()}


def cmd_base(_args):
    if BASE.exists():
        shutil.rmtree(BASE)
    BASE.mkdir(parents=True)
    git(BASE, "init", "-q", "-b", TRUNK)
    configure_repo(BASE)
    heads = server_heads()
    have = existing(BASE, list(set(heads.values())))
    desired = {r: s for r, s in remote_tracking(heads).items() if s in have}
    desired[f"refs/heads/{TRUNK}"] = heads[TRUNK]
    update_refs(BASE, desired, ["refs/heads", "refs/remotes"])
    git(BASE, "checkout", "-q", "-f", TRUNK)
    git(BASE, "status", "--porcelain")  # warm the index stat cache
    print(json.dumps({"base": str(BASE), "remote_tracking": len(desired) - 1}))


def case_dir(name):
    return SNAPSHOTS / name


def worktree_slug(i, branch):
    return f"w{i:03d}-" + branch.replace("/", "-")[:40]


def clone_tree(src, dst):
    subprocess.run(["cp", "-cRp", str(src), str(dst)], check=True)


def dirty(wt, kind):
    if kind == "tracked":
        files = [f for f in git(wt, "ls-files", "--", "*.md").splitlines()[:2]]
        for f in files:
            with open(wt / f, "a") as fh:
                fh.write("\nperf-lab local change\n")
        git(wt, "add", "--", files[0])
    elif kind == "untracked":
        scratch = wt / "perf-scratch"
        scratch.mkdir()
        for i in range(3):
            (scratch / f"note-{i}.txt").write_text(("untracked perf note %d\n" % i) * 200)


def write_fake_bb(cdir, spec, worktree_paths, shell_paths, remote_identity_url):
    bb = cdir / "bb"
    bb.mkdir(exist_ok=True)
    rows = []
    owned = [b for b in spec.branches if b.worktree and b.bb_owned]
    for i, b in enumerate(owned):
        rows.append((f"env_lab_c{i:03d}", worktree_paths[b.name], [f"thr_lab_c{i:03d}_{k}" for k in range(3)]))
    for i, path in enumerate(shell_paths[: max(0, BB_REGISTRY_ROWS - len(rows))]):
        rows.append((f"env_lab_s{i:03d}", path, []))
    registry = [{
        "id": env_id, "name": None, "projectId": "proj_lab", "hostId": "host_lab", "path": str(path),
        "isGitRepo": True, "isWorktree": True, "branchName": None, "status": "ready",
        "environmentProviderId": "git-worktree", "managed": True, "workspaceProvisionType": "managed-worktree",
        "lifecycle": {"phase": "active", "retireAt": None, "teardown": None},
    } for env_id, path, _ in rows]
    threads = {env_id: [{"id": t, "status": "completed", "environmentId": env_id} for t in tids]
               for env_id, _, tids in rows}
    (bb / "registry.json").write_text(json.dumps({
        "status": {"thread": {"id": "thr_lab_current", "environment": {"hostId": "host_lab",
                   "display": {"id": "env_lab_current"}}}},
        "environments": registry,
        "providers": [{"id": "git-worktree"}, {"id": "project-checkout"}],
        "project": {"id": "proj_lab", "gitRemoteUrl": remote_identity_url},
        "threads": threads,
        "main_repo": str(cdir / "repo"),
    }, indent=1))


def cmd_build(args):
    prs = json.loads((FORGE / "prs.json").read_text())
    heads = server_heads()
    have = existing(BASE, [p["head"]["sha"] for p in prs])
    pools = select_pools(prs, heads, have)
    live_main = git(LIVE, "rev-parse", f"refs/heads/{TRUNK}").strip()
    for name in args.cases:
        build_case(name, case_spec(name, pools, heads[TRUNK], live_main))


def build_case(name, spec):
    cdir = case_dir(name)
    if cdir.exists():
        subprocess.run(["rm", "-rf", str(cdir)], check=True)
    cdir.mkdir(parents=True)
    repo = cdir / "repo"
    clone_tree(BASE, repo)
    configure_repo(repo)

    if spec.live_refs:
        desired = {}
        for line in git(LIVE, "for-each-ref", "--format=%(refname) %(objectname)").splitlines():
            ref, oid = line.split()
            desired[ref] = oid
        update_refs(repo, desired, [])
        git(repo, "checkout", "-q", "-f", TRUNK)
    else:
        # Remote-tracking refs come from the base (server heads whose objects exist).
        update_refs(repo, {f"refs/heads/{TRUNK}": spec.trunk_local}, ["refs/heads"])
        trunk_tip = spec.trunk_local
        for b in spec.branches:
            parent_rev = git(repo, "merge-base", trunk_tip, b.tip).strip()
            meta = metadata_json(b.parent, parent_rev,
                                 b.pr["number"] if b.pr_info_state else None, b.pr_info_state,
                                 bool(b.pr and b.pr.get("draft")))
            blob = git(repo, "hash-object", "-w", "--stdin", input=meta).strip()
            git(repo, "update-ref", f"refs/heads/{b.name}", b.tip)
            git(repo, "update-ref", f"refs/branch-metadata/{b.name}", blob)
        # Same trunk-marker blob as live (object read through alternates).
        git(repo, "update-ref", "refs/stax/trunk", git(LIVE, "rev-parse", "refs/stax/trunk").strip())
    git(repo, "status", "--porcelain")

    # Linked worktrees: real (CoW-cloned) checkouts for cleanup candidates,
    # registered shells (admin dir + .git file, detached at trunk) for the rest.
    worktree_paths, wt_root = {}, cdir / "wt"
    wt_root.mkdir()
    for i, b in enumerate(b for b in spec.branches if b.worktree):
        path = wt_root / worktree_slug(i, b.name)
        git(repo, "worktree", "add", "-q", "--no-checkout", str(path), b.name)
        for entry in repo.iterdir():
            if entry.name != ".git":
                clone_tree(entry, path / entry.name)
        admin = Path(git(path, "rev-parse", "--git-dir").strip())
        shutil.copy2(repo / ".git" / "index", admin / "index")
        # Refresh stat data first so `reset --hard` rewrites only files that
        # differ from trunk; the rest stay copy-on-write clones.
        git(path, "update-index", "-q", "--refresh", check=False)
        git(path, "reset", "-q", "--hard", b.name)
        dirty(path, b.worktree)
        git(path, "status", "--porcelain")
        worktree_paths[b.name] = path
    shell_root, shell_paths = cdir / "shells", []
    shell_root.mkdir()
    for i in range(SHELL_WORKTREES - len(worktree_paths)):
        path = shell_root / f"s{i:03d}"
        git(repo, "worktree", "add", "-q", "--no-checkout", "--detach", str(path), spec.trunk_local)
        shell_paths.append(path)

    remote_identity_url = git(repo, "remote", "get-url", "origin").strip()
    write_fake_bb(cdir, spec, worktree_paths, shell_paths, remote_identity_url)
    forge_prs = {}
    for b in spec.branches:
        if b.pr is not None:
            forge_prs[str(b.pr["number"])] = b.pr
    (cdir / "forge-prs.json").write_text(json.dumps(forge_prs))

    home = cdir / "home"
    (home / ".config" / "stax").mkdir(parents=True)
    (home / ".gitconfig").write_text("[user]\n\tname = Perf Lab\n\temail = perf-lab@example.invalid\n")
    spec_json = asdict(spec)
    spec_json["branches"] = [{k: v for k, v in asdict(b).items() if k != "pr"} | {"pr": b.pr and b.pr["number"]}
                             for b in spec.branches]
    spec_json["worktrees"] = {k: str(v) for k, v in worktree_paths.items()}
    spec_json["shells"] = len(shell_paths)
    (cdir / "spec.json").write_text(json.dumps(spec_json, indent=1))
    save_golden(cdir)
    print(json.dumps({"case": name, "branches": len(spec.branches), "worktrees": len(worktree_paths),
                      "shells": len(shell_paths), "fingerprint": fingerprint(cdir)["digest"]}))


def stax_config(api_base_url):
    return (f'[remote]\nname = "origin"\nforge = "gitea"\nbase_url = "https://git.wonderly.info"\n'
            f'api_base_url = "{api_base_url}"\n\n[submit]\nstack_links = "off"\n\n[ui]\ntips = false\n')


def save_golden(cdir):
    golden = cdir / "golden"
    if golden.exists():
        subprocess.run(["rm", "-rf", str(golden)], check=True)
    golden.mkdir()
    rsync(cdir / "repo" / ".git", golden / "gitdir", exclude_objects=True)
    rsync(cdir / "shells", golden / "shells")
    rsync(cdir / "home", golden / "home")
    if (cdir / "wt").exists():
        clone_tree(cdir / "wt", golden / "wt")
    (golden / "fingerprint.json").write_text(json.dumps(fingerprint(cdir), indent=1))


def rsync(src, dst, exclude_objects=False):
    cmd = ["rsync", "-a", "--delete"]
    if exclude_objects:
        cmd += ["--exclude", "/objects/"]
    subprocess.run(cmd + [f"{src}/", f"{dst}/"], check=True)


def reset(cdir, verify=True):
    golden = cdir / "golden"
    rsync(golden / "gitdir", cdir / "repo" / ".git", exclude_objects=True)
    rsync(golden / "shells", cdir / "shells")
    rsync(golden / "home", cdir / "home")
    bbstate = cdir / "bb" / "state.json"
    if bbstate.exists():
        bbstate.unlink()
    if (golden / "wt").exists():
        def restore(wt):
            target = cdir / "wt" / wt.name
            if target.exists():
                subprocess.run(["rm", "-rf", str(target)], check=True)
            clone_tree(wt, target)
            # Clones carry new inodes/ctimes; refresh the index stat cache so
            # the timed run sees a warm index like a long-lived checkout.
            git(target, "update-index", "-q", "--refresh", check=False)
        with ThreadPoolExecutor(max_workers=6) as pool:
            list(pool.map(restore, sorted((golden / "wt").iterdir())))
    repo = cdir / "repo"
    if git(repo, "status", "--porcelain", "--untracked-files=all"):
        git(repo, "reset", "-q", "--hard")
        git(repo, "clean", "-q", "-fd")
    if verify:
        got, want = fingerprint(cdir), json.loads((golden / "fingerprint.json").read_text())
        if got["digest"] != want["digest"]:
            diff = [k for k in want["parts"] if want["parts"][k] != got["parts"].get(k)]
            raise SystemExit(f"reset of {cdir.name} did not reproduce golden state; differing: {diff}")
        return got
    return None


def fingerprint(cdir):
    """Outcome fingerprint: refs (excluding stax receipts/recovery refs, which
    embed operation IDs), worktree inventory and per-worktree status."""
    repo = cdir / "repo"
    refs = git(repo, "for-each-ref", "--format=%(refname) %(objectname)").splitlines()
    kept = sorted(r for r in refs if not r.startswith(("refs/stax/backups/", "refs/stax/recovery", "refs/stax/ops",
                                                        "refs/stax/receipts", "refs/stax/undo")))
    stax_refs = len([r for r in refs if r.startswith("refs/stax/")])
    worktrees = git(repo, "worktree", "list", "--porcelain").replace(str(cdir), "<case>")
    statuses = {}
    for block in git(repo, "worktree", "list", "--porcelain").split("\n\n"):
        lines = block.strip().splitlines()
        if not lines:
            continue
        path = Path(lines[0].split(" ", 1)[1])
        if path.parent.name == "shells":
            continue  # shells have no checkout; their inventory is covered above
        if path.exists():
            # A teardown interrupted mid-removal leaves a directory that is no
            # longer a Git worktree; record that outcome instead of failing.
            proc = subprocess.run(["git", "status", "--porcelain=v1", "--untracked-files=all"], cwd=path,
                                  capture_output=True, text=True, check=False)
            statuses[str(path).replace(str(cdir), "<case>")] = (
                proc.stdout if proc.returncode == 0 else "<not a git worktree>")
    parts = {
        "refs_sha": hashlib.sha256("\n".join(kept).encode()).hexdigest(),
        "refs_count": len(kept),
        "heads": sorted(r.split()[0] for r in kept if r.startswith("refs/heads/")),
        "metadata_count": len([r for r in kept if r.startswith("refs/branch-metadata/")]),
        "worktrees_sha": hashlib.sha256(worktrees.encode()).hexdigest(),
        "worktree_count": worktrees.count("worktree "),
        "status": {k: hashlib.sha256(v.encode()).hexdigest() for k, v in statuses.items()},
    }
    return {"digest": fingerprint_digest(parts), "parts": parts, "stax_refs": stax_refs}


def cmd_reset(args):
    for name in args.cases:
        got = reset(case_dir(name))
        print(json.dumps({"case": name, "reset": "ok", "digest": got["digest"]}))


def cmd_fingerprint(args):
    for name in args.cases:
        print(json.dumps(fingerprint(case_dir(name)), indent=1))


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("origin").set_defaults(fn=cmd_origin)
    sub.add_parser("base").set_defaults(fn=cmd_base)
    for cmd, fn in (("build", cmd_build), ("reset", cmd_reset), ("fingerprint", cmd_fingerprint)):
        p = sub.add_parser(cmd)
        p.add_argument("cases", nargs="+", choices=CASES)
        p.set_defaults(fn=fn)
    sub.add_parser("list").set_defaults(fn=lambda _a: print("\n".join(CASES)))
    args = ap.parse_args()
    args.fn(args)


if __name__ == "__main__":
    sys.exit(main())
