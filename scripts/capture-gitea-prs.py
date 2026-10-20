#!/usr/bin/env python3
"""Bounded, read-only capture of Gitea pull requests for the sync replay forge.

Uses `tea api` (tea owns its stored login; this script never reads or writes a
credential). Only GET requests are issued. Each recorded PR keeps the fields
stax's GiteaPull model reads plus merge provenance; the PR body is replaced by
filler of equal length and `pad` preserves the original object's byte size so
replayed payload sizes stay representative. No headers are recorded.

Usage:
  capture-gitea-prs.py --repo OWNER/REPO --out prs.json \
      [--open-pages 8] [--closed-pages 6] [--latency-samples 20] [--latency-out lat.tsv]
"""

import argparse
import json
import subprocess
import time

KEEP = (
    "number", "state", "title", "draft", "mergeable", "merged", "merged_at",
    "merge_commit_sha", "html_url", "created_at", "updated_at", "closed_at",
)


def tea_get(path):
    started = time.monotonic()
    out = subprocess.run(["tea", "api", path], check=True, capture_output=True)
    return json.loads(out.stdout), time.monotonic() - started, len(out.stdout)


def branch_ref(ref):
    ref = ref or {}
    return {"ref": ref.get("ref"), "sha": ref.get("sha"), "label": ref.get("label")}


def scrub(pr):
    body = pr.get("body") or ""
    kept = {k: pr.get(k) for k in KEEP}
    kept["body"] = "x" * len(body)
    kept["head"] = branch_ref(pr.get("head"))
    kept["base"] = branch_ref(pr.get("base"))
    user = pr.get("user") or {}
    kept["user"] = {"login": user.get("login")}
    original = len(json.dumps(pr, separators=(",", ":")))
    kept["pad"] = "p" * max(0, original - len(json.dumps(kept, separators=(",", ":"))))
    return kept


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", required=True)
    ap.add_argument("--out")
    ap.add_argument("--pages-out")
    ap.add_argument("--open-pages", type=int, default=8)
    ap.add_argument("--closed-pages", type=int, default=6)
    ap.add_argument("--latency-samples", type=int, default=20)
    ap.add_argument("--latency-out")
    ap.add_argument("--status-shas", help="file of commit SHAs whose CI statuses to record")
    ap.add_argument("--status-out")
    args = ap.parse_args()

    if args.status_shas:
        # Commit statuses for the replay forge's CI-history path; the rest of
        # the capture is skipped. Only fields stax's GiteaCommitStatus reads
        # are kept, padded to the original byte size.
        statuses = {}
        for sha in open(args.status_shas).read().split():
            body, _, _ = tea_get(f"/repos/{args.repo}/commits/{sha}/statuses?limit=50")
            rows = []
            for st in body:
                kept = {k: st.get(k) for k in ("context", "status", "target_url", "created_at", "updated_at")}
                original = len(json.dumps(st, separators=(",", ":")))
                kept["pad"] = "p" * max(0, original - len(json.dumps(kept, separators=(",", ":"))))
                rows.append(kept)
            statuses[sha] = rows
        with open(args.status_out, "w") as f:
            json.dump(statuses, f)
        print(json.dumps({"statuses": len(statuses), "rows": sum(len(v) for v in statuses.values())}))
        return

    prs, pages = {}, []
    for state, count in (("open", args.open_pages), ("closed", args.closed_pages)):
        for page in range(1, count + 1):
            path = f"/repos/{args.repo}/pulls?state={state}&sort=recentupdate&limit=50&page={page}"
            body, secs, size = tea_get(path)
            pages.append({"state": state, "page": page, "count": len(body), "secs": round(secs, 3), "bytes": size})
            for pr in body:
                prs[pr["number"]] = scrub(pr)
            if len(body) < 50:
                break

    with open(args.out, "w") as f:
        json.dump(sorted(prs.values(), key=lambda p: p["number"]), f, indent=1)
    with open(args.pages_out, "w") as f:
        json.dump(pages, f, indent=1)

    if args.latency_out and args.latency_samples:
        # Single-PR GET latency (the endpoint sync refreshes), plus tea's own
        # startup cost (`tea --version`) so request time can be separated.
        numbers = sorted(prs)[-args.latency_samples:]
        rows = ["kind\tnumber\tsecs\tbytes"]
        for n in numbers:
            _, secs, size = tea_get(f"/repos/{args.repo}/pulls/{n}")
            rows.append(f"get_pr\t{n}\t{secs:.3f}\t{size}")
        for _ in range(10):
            started = time.monotonic()
            subprocess.run(["tea", "--version"], check=True, capture_output=True)
            rows.append(f"tea_startup\t-\t{time.monotonic() - started:.3f}\t0")
        with open(args.latency_out, "w") as f:
            f.write("\n".join(rows) + "\n")

    print(json.dumps({"prs": len(prs), "pages": len(pages)}))


if __name__ == "__main__":
    main()
