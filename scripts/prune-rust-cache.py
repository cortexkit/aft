#!/usr/bin/env python3
"""Keep at most two recent main caches per compatible Linux Rust build."""

from __future__ import annotations

from collections import defaultdict
from datetime import datetime
import json
import os
import re
from urllib.parse import urlencode
from urllib.request import Request, urlopen


MAIN = "refs/heads/main"
LANES = (
    "linux-archive-test-wasm-hooks",
    "linux-conformance-dev-test-wasm-timing-hooks",
    "linux-plugin-e2e-dev-wasm",
    "linux-worker-landlock-dev-test-wasm-hooks",
    "linux-sandbox-landlock-dev-test-wasm-timing-hooks",
    "linux-storm-ci-storm-off-wasm-hooks",
    "linux-storm-ci-storm-thin-wasm-hooks",
    "linux-msrv-dev-check-wasm-hooks",
    "linux-opencode2-stage-wasm-hooks",
    "linux-search-ci-storm-wasm",
)
PAGE_SIZE = 100
API_ERRORS = (OSError, AttributeError, KeyError, TypeError, ValueError)


def request(method: str, endpoint: str, **params):
    base = os.environ["GITHUB_API_URL"]
    repo = os.environ["GITHUB_REPOSITORY"]
    query = "?" + urlencode(params) if params else ""
    req = Request(
        f"{base}/repos/{repo}{endpoint}{query}",
        method=method,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {os.environ['GH_TOKEN']}",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urlopen(req, timeout=10) as response:
        body = response.read()
        return json.loads(body) if body else None


def prune_lane(api, lane_prefix: str, started: datetime) -> None:
    # In the pinned Rust Cache action, the restore key ends with the
    # compiler/environment hash; only the final hash varies with Cargo.lock.
    key_pattern = re.compile(
        rf"({re.escape(lane_prefix)}[A-Za-z]+-[A-Za-z0-9_]+-[0-9a-f]{{8}})-[0-9a-f]{{8}}"
    )
    compatible = defaultdict(dict)
    page = 1
    while True:
        entries = api("GET", "/actions/caches", key=lane_prefix,
                      ref=MAIN, per_page=PAGE_SIZE, page=page)["actions_caches"]
        for entry in entries:
            key = key_pattern.fullmatch(entry["key"])
            if key and entry["ref"] == MAIN and type(entry["id"]) is int:
                compatible[key[1]][entry["id"]] = entry
        if len(entries) < PAGE_SIZE:
            break
        page += 1

    for prefix, entries_by_id in sorted(compatible.items()):
        dated = [(datetime.fromisoformat(entry["created_at"].replace("Z", "+00:00")), cache_id, entry)
                 for cache_id, entry in entries_by_id.items()]
        dated.sort(reverse=True)
        # A full quota can reject a save. In that case keep the last working
        # baseline rather than treating an old restore as a fresh entry.
        if dated[0][0] < started:
            print(f"No new main cache since this run started: {prefix}")
            continue
        for _, _, entry in dated[:2]:
            print(f"Kept {entry['key']} ({entry['size_in_bytes']} bytes)")
        for _, cache_id, entry in dated[2:]:
            try:
                api("DELETE", f"/actions/caches/{cache_id}")
            except API_ERRORS as exc:
                print(f"Cache prune warning: could not delete {entry['key']}: {exc}")
            else:
                print(f"Deleted {entry['key']} ({entry['size_in_bytes']} bytes)")
    if not compatible:
        print(f"No main caches found for {lane_prefix}")


def main(api=request) -> None:
    if os.environ.get("GITHUB_REF") != MAIN:
        print("Cache prune skipped: not on main")
        return
    namespace = os.environ.get("RUST_CACHE_NAMESPACE", "")
    if not re.fullmatch(r"aft-rust-v1-[0-9a-f]{64}", namespace):
        print("Cache prune warning: missing manifest hash, nothing deleted")
        return
    try:
        run_id = int(os.environ["GITHUB_RUN_ID"])
        attempt = int(os.environ["GITHUB_RUN_ATTEMPT"])
        run = api("GET", f"/actions/runs/{run_id}/attempts/{attempt}")
        started = datetime.fromisoformat(run["run_started_at"].replace("Z", "+00:00"))
    except API_ERRORS as exc:
        print(f"Cache prune warning: could not establish run start time: {exc}")
    else:
        for lane in LANES:
            try:
                prune_lane(api, f"{namespace}-{lane}-", started)
            except API_ERRORS as exc:
                print(f"Cache prune warning: could not prune {lane}: {exc}")
    try:
        usage = api("GET", "/actions/cache/usage")
        print(f"Repository cache total: {usage['active_caches_count']} entries, "
              f"{usage['active_caches_size_in_bytes']} bytes")
    except API_ERRORS as exc:
        print(f"Cache prune warning: could not read repository total: {exc}")


if __name__ == "__main__":
    main()
