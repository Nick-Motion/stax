#!/usr/bin/env python3
"""Deterministic Gitea replay forge for sync benchmarks.

Serves recorded, credential-free PR objects (`forge-prs.json` written by
motion-net-perf-lab.py from capture-gitea-prs.py output) at
GET /api/v1/repos/{owner}/{repo}/pulls/{n}. Every request is appended to the
JSONL log with start/end wall time so concurrency and serial latency are
visible. Per-request latency is a fixed sleep (`--latency-ms`); HTTP/1.1
keep-alive is supported so client connection reuse is preserved.

Control (harness only): POST /__control/load?case=DIR&latency_ms=N swaps the
served case and latency without restarting.
"""

import argparse
import json
import re
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

PULL = re.compile(r"^/api/v1/repos/[^/]+/[^/]+/pulls/(\d+)$")
STATUSES = re.compile(r"^/api/v1/repos/[^/]+/[^/]+/commits/([0-9a-f]{40})/statuses$")


class State:
    def __init__(self, log_path, statuses_path=None):
        self.lock = threading.Lock()
        self.prs = {}
        self.latency = 0.0
        self.log = open(log_path, "a", buffering=1)
        self.statuses = {}
        if statuses_path:
            with open(statuses_path) as fh:
                self.statuses = {k: json.dumps(v).encode() for k, v in json.load(fh).items()}

    def load(self, case_dir, latency_ms):
        with open(f"{case_dir}/forge-prs.json") as fh:
            prs = json.load(fh)
        with self.lock:
            self.prs = {int(k): json.dumps(v).encode() for k, v in prs.items()}
            self.latency = latency_ms / 1000.0


class ReplayServer(ThreadingHTTPServer):
    # socketserver's default listen backlog is 5. Sync refreshes PRs with 8
    # concurrent connections, so a short backlog refuses some of them, which
    # shows up as missing PR observations (and an undercounted request log)
    # rather than as server errors.
    request_queue_size = 128
    daemon_threads = True


def handler(state):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def reply(self, status, body):
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            url = urlparse(self.path)
            if url.path == "/__control/load":
                q = parse_qs(url.query)
                state.load(q["case"][0], float(q.get("latency_ms", ["0"])[0]))
                self.reply(200, b'{"ok":true}')
            else:
                self.reply(405, b'{"message":"replay forge is read-only"}')

        def do_GET(self):
            started = time.time()
            if state.latency:
                time.sleep(state.latency)
            path = urlparse(self.path).path
            match = PULL.match(path)
            status_match = STATUSES.match(path)
            if match:
                body, endpoint = state.prs.get(int(match.group(1))), "GET /pulls/{n}"
            elif status_match:
                # Unrecorded commits have no statuses, as on the real forge.
                body, endpoint = state.statuses.get(status_match.group(1), b"[]"), "GET /commits/{sha}/statuses"
            else:
                body, endpoint = None, f"GET {path}"
            status = 200 if body is not None else 404
            # Log before replying so a run's requests are on disk before the
            # client can observe the response and exit.
            state.log.write(json.dumps({"start": started, "end": time.time(), "endpoint": endpoint,
                                        "path": path, "status": status,
                                        "conn": self.client_address[1]}) + "\n")
            self.reply(status, body if body is not None else b'{"message":"not recorded"}')

    return Handler


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=0)
    ap.add_argument("--log", required=True)
    ap.add_argument("--port-file", required=True)
    ap.add_argument("--statuses")
    args = ap.parse_args()
    state = State(args.log, args.statuses)
    server = ReplayServer(("127.0.0.1", args.port), handler(state))
    with open(args.port_file, "w") as fh:
        fh.write(str(server.server_address[1]))
    server.serve_forever()


if __name__ == "__main__":
    main()
