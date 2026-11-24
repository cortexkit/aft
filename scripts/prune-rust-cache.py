#!/usr/bin/env python3
"""Keep at most two current main caches per Linux Rust build lane."""

from __future__ import annotations

from datetime import datetime
import json
import os
import re
from typing import NamedTuple
from urllib.parse import urlencode
from urllib.request import Request, urlopen


MAIN = "refs/heads/main"
CACHE_FAMILY = "aft-rust-v"
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


def list_main_caches(api) -> list[dict]:
    # Include every version of the namespace, including old hashed keys.
    by_id = {}
    page = 1
    while True:
        entries = api("GET", "/actions/caches", key=CACHE_FAMILY,
                      ref=MAIN, per_page=PAGE_SIZE, page=page)["actions_caches"]
        for entry in entries:
            if entry["ref"] == MAIN and type(entry["id"]) is int:
                by_id[entry["id"]] = entry
        if len(entries) < PAGE_SIZE:
            return list(by_id.values())
        page += 1


class LaneCache(NamedTuple):
    created_at: datetime
    cache_id: int
    prefix: str
    namespace: str
    key: str
    size: int


def prune_lane(api, namespace: str, lane: str, listed: list[dict], started: datetime) -> None:
    # The pinned action puts the compiler/environment digest before the
    # manifest/lockfile digest. Match the whole key so lanes cannot overlap.
    key_pattern = re.compile(
        rf"(?P<prefix>(?P<namespace>{CACHE_FAMILY}(?P<version>[0-9]+)(?:-[0-9a-f]{{64}})?)"
        rf"-{re.escape(lane)}-[A-Za-z]+-[A-Za-z0-9_]+-[0-9a-f]{{8}})-[0-9a-f]{{8}}"
    )
    namespace_version = int(namespace.removeprefix(CACHE_FAMILY))
    candidates = []
    for entry in listed:
        key = key_pattern.fullmatch(entry["key"])
        # An older main run can finish after a newer namespace starts saving.
        if not key or int(key["version"]) > namespace_version:
            continue
        candidates.append(LaneCache(
            datetime.fromisoformat(entry["created_at"].replace("Z", "+00:00")),
            entry["id"], key["prefix"], key["namespace"], entry["key"], entry["size_in_bytes"],
        ))
    candidates.sort(key=lambda entry: (entry.created_at, entry.cache_id), reverse=True)
    current = next((entry for entry in candidates if entry.namespace == namespace), None)
    # A rejected save must not remove the last usable cache for this lane.
    if current is None or current.created_at < started:
        print(f"No new main cache since this run started: {namespace}-{lane}")
        return

    current_entries = [entry for entry in candidates if entry.prefix == current.prefix]
    kept = {entry.cache_id for entry in current_entries[:2]}
    for entry in candidates:
        if entry.cache_id in kept:
            print(f"Kept {entry.key} ({entry.size} bytes)")
            continue
        try:
            api("DELETE", f"/actions/caches/{entry.cache_id}")
        except API_ERRORS as exc:
            print(f"Cache prune warning: could not delete {entry.key}: {exc}")
        else:
            print(f"Deleted {entry.key} ({entry.size} bytes)")


def main(api=request) -> None:
    if os.environ.get("GITHUB_REF") != MAIN:
        print("Cache prune skipped: not on main")
        return
    namespace = os.environ.get("RUST_CACHE_NAMESPACE", "")
    if not re.fullmatch(rf"{CACHE_FAMILY}[0-9]+", namespace):
        print("Cache prune warning: invalid Rust cache namespace, nothing deleted")
        return
    try:
        run_id = int(os.environ["GITHUB_RUN_ID"])
        attempt = int(os.environ["GITHUB_RUN_ATTEMPT"])
        run = api("GET", f"/actions/runs/{run_id}/attempts/{attempt}")
        started = datetime.fromisoformat(run["run_started_at"].replace("Z", "+00:00"))
    except API_ERRORS as exc:
        print(f"Cache prune warning: could not establish run start time: {exc}")
    else:
        try:
            listed = list_main_caches(api)
        except API_ERRORS as exc:
            print(f"Cache prune warning: could not list main Rust caches: {exc}")
        else:
            for lane in LANES:
                try:
                    prune_lane(api, namespace, lane, listed, started)
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
