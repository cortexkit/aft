#!/usr/bin/env python3
"""Replay aft_search queries against two AFT builds and compare every reply.

`compare_data_layer.py` compares one binary in two modes. This compares two
binaries (`--before` and `--after`) in the same mode, for a change that must
not alter any reply: each build answers every query of `--queries` (one per
line, blank and repeated lines dropped) on the same tree, each from its own
empty storage, semantic search off, with the same warm-up first and after the
file watcher went quiet. Replies are masked and compared whole exactly as
`compare_data_layer.py` does (request id and snapshot generation masked,
timing-driven status lines counted apart as process noise).

`--mode legacy` runs both builds on today's indexes, `--mode data` both with
`AFT_DATA_LAYER=1`. Writes a JSON report (`--out`) and exits nonzero when any
reply differs outside process noise.
"""

from __future__ import annotations

import argparse
import json
import sys
import tempfile
import time
from pathlib import Path

from compare_data_layer import WARM_UP_QUERY, Process, compare, latency, read_replies, replay, send_search
from run_real_query import AftProtocolError
from search_quality_lib import canonical_json


def settle(process: Process, timeout: float) -> None:
    """Wait for the watcher to go quiet. Nothing edits the tree during a
    replay, so when the status call itself times out (a loaded machine), a
    short pause stands in for it; a watcher that was still busy would show as
    process noise in the comparison, never as a silent pass."""
    try:
        process.settle(timeout)
    except AftProtocolError as error:
        print(f"settle: {str(error)[:120]}; pausing instead", file=sys.stderr)
        time.sleep(15.0)


def run_build(binary: Path, root: Path, run_dir: Path, mode: str, rows: list, timeout: float) -> tuple[Path, list]:
    run_dir.mkdir(parents=True, exist_ok=True)
    replies = run_dir / "replies.jsonl"
    process = Process(binary, root, run_dir, mode)
    try:
        process.configure_without_semantic(timeout)
        process.warm({"query": WARM_UP_QUERY}, send_search, timeout)
        settle(process, timeout)
        measures = replay(process, rows, send_search, replies)
    finally:
        process.close()
    return replies, measures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--before", required=True, help="the build without the change")
    parser.add_argument("--after", required=True, help="the build with the change")
    parser.add_argument("--root", required=True, help="the tree to search")
    parser.add_argument("--queries", required=True, help="one query per line")
    parser.add_argument("--mode", choices=("legacy", "data"), default="legacy")
    parser.add_argument("--limit", type=int, default=None, help="replay at most N queries")
    parser.add_argument("--out", required=True)
    parser.add_argument("--ready-timeout", type=float, default=900.0)
    args = parser.parse_args()

    queries: list[str] = []
    seen: set[str] = set()
    for line in Path(args.queries).read_text(encoding="utf-8").splitlines():
        query = line.strip()
        if query and query not in seen:
            seen.add(query)
            queries.append(query)
    queries = queries[: args.limit]
    rows = [{"id": f"query:{index}", "request": {"query": query}} for index, query in enumerate(queries)]
    root = Path(args.root).resolve()

    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="aft-build-parity-") as directory:
        before, before_measures = run_build(
            Path(args.before).resolve(), root, Path(directory) / "before", args.mode, rows, args.ready_timeout
        )
        after, after_measures = run_build(
            Path(args.after).resolve(), root, Path(directory) / "after", args.mode, rows, args.ready_timeout
        )
        compared = compare(rows, read_replies(before), read_replies(after))
    report = {
        "mode": args.mode,
        "rows": len(rows),
        "before_vs_after": compared,
        "latency": {"before": latency(before_measures), "after": latency(after_measures)},
        "seconds": round(time.monotonic() - started, 1),
    }
    output = Path(args.out)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(canonical_json(report))
    print(
        f"{args.mode}: {compared['differing']} of {compared['rows']} replies differ, "
        f"{compared['process_noise']} only in process noise, "
        f"{compared['anchored_examined_count']} only in the anchored examined count; "
        f"latency {json.dumps(report['latency'])}"
    )
    return 1 if compared["differing"] > 0 or compared["anchored_examined_count"] > 0 else 0


if __name__ == "__main__":
    sys.exit(main())
