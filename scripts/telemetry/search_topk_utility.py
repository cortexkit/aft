#!/usr/bin/env python3
"""Was a large `aft_search` reply actually used?

For every `aft_search` call in the window, parse the ranked file list out of the
rendered reply, then look at the next few tool calls in the same session and ask
whether any of them touched a file the search returned — and at what rank. A
reply whose used paths all sit in the first few ranks is paying for rows nobody
reads.

Read-only against the live OpenCode database; writes nothing.
"""

from __future__ import annotations

import json
import re
import sqlite3
import sys
from collections import Counter, defaultdict
from pathlib import Path

DB = Path.home() / ".local/share/opencode/opencode.db"
WINDOW_DAYS = 7
FOLLOW_UPS = 3
# Rendered rows start at column 0 with a path; a result block looks like
#   crates/aft/src/foo.rs [exact]
#     symbol [function] lines 10-20
RESULT_ROW = re.compile(r"^(?P<path>[A-Za-z0-9_./-]+\.[A-Za-z0-9_]+)(?:\s|$)")


def ranked_paths(output: str) -> list[str]:
    seen: list[str] = []
    for line in output.splitlines():
        match = RESULT_ROW.match(line)
        if not match:
            continue
        path = match.group("path")
        if path not in seen:
            seen.append(path)
    return seen


def paths_in_input(payload: dict) -> set[str]:
    """Every filesystem-looking string in a tool call's arguments."""
    found: set[str] = set()

    def walk(value: object) -> None:
        if isinstance(value, str):
            for token in re.findall(r"[A-Za-z0-9_./-]+\.[A-Za-z0-9_]+", value):
                found.add(token)
        elif isinstance(value, dict):
            for item in value.values():
                walk(item)
        elif isinstance(value, list):
            for item in value:
                walk(item)

    walk(payload)
    return found


def main() -> int:
    connection = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
    connection.execute("PRAGMA busy_timeout = 20000")
    cursor = connection.execute(
        """
        SELECT session_id, time_created, data
        FROM part
        WHERE time_created > (strftime('%s','now') - ? * 86400) * 1000
          AND json_extract(data,'$.tool') IS NOT NULL
        ORDER BY session_id, time_created
        """,
        (WINDOW_DAYS,),
    )

    by_session: dict[str, list[tuple[int, dict]]] = defaultdict(list)
    for session_id, created, raw in cursor:
        try:
            payload = json.loads(raw)
        except json.JSONDecodeError:
            continue
        by_session[session_id].append((created, payload))

    # rank bucket -> number of searches whose deepest used row fell in it
    deepest_used: Counter[str] = Counter()
    # requested topK -> [rows returned, rows used]
    per_topk: dict[int, list[int]] = defaultdict(lambda: [0, 0, 0])
    unused_large = 0
    large_total = 0

    for calls in by_session.values():
        for index, (_, payload) in enumerate(calls):
            if payload.get("tool") != "aft_search":
                continue
            state = payload.get("state") or {}
            output = state.get("output")
            if not isinstance(output, str):
                continue
            top_k = (state.get("input") or {}).get("topK") or 10
            try:
                top_k = int(top_k)
            except (TypeError, ValueError):
                top_k = 10
            rows = ranked_paths(output)
            if not rows:
                continue

            follow_paths: set[str] = set()
            for _, later in calls[index + 1 : index + 1 + FOLLOW_UPS]:
                if later.get("tool") == "aft_search":
                    continue
                follow_paths |= paths_in_input((later.get("state") or {}).get("input") or {})

            used_ranks = [
                rank
                for rank, path in enumerate(rows, start=1)
                if any(path.endswith(candidate) or candidate.endswith(path) for candidate in follow_paths)
            ]
            per_topk[top_k][0] += 1
            per_topk[top_k][1] += len(rows)
            per_topk[top_k][2] += len(used_ranks)

            if top_k >= 50:
                large_total += 1
                if not used_ranks:
                    unused_large += 1

            if used_ranks:
                deepest = max(used_ranks)
                bucket = (
                    "1-5"
                    if deepest <= 5
                    else "6-10"
                    if deepest <= 10
                    else "11-20"
                    if deepest <= 20
                    else "21-50"
                    if deepest <= 50
                    else "51+"
                )
                deepest_used[bucket] += 1
            else:
                deepest_used["none used"] += 1

    print(f"{'topK':>6} {'searches':>9} {'rows/reply':>11} {'used/reply':>11}")
    for top_k in sorted(per_topk):
        searches, rows, used = per_topk[top_k]
        print(f"{top_k:>6} {searches:>9} {rows / searches:>11.1f} {used / searches:>11.2f}")

    print("\ndeepest rank the session actually touched, next 3 calls:")
    order = ["1-5", "6-10", "11-20", "21-50", "51+", "none used"]
    total = sum(deepest_used.values()) or 1
    for bucket in order:
        count = deepest_used.get(bucket, 0)
        print(f"  {bucket:>9} {count:>7} ({100 * count / total:4.1f}%)")

    if large_total:
        share = 100 * unused_large / large_total
        print(f"\ntopK>=50 replies with no path used in the next {FOLLOW_UPS} calls: {unused_large}/{large_total} ({share:.1f}%)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
