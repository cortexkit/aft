"""Compare baseline/candidate public replies, CPU cost and latency captures.

Only the temporary corpus prefix (normalized by the runner) and the random
process nonce in snapshot_generation are excluded from reply-byte equality.
"""
from __future__ import annotations

import argparse
from copy import deepcopy
import gzip
import hashlib
import json
from pathlib import Path

from compare_engine_only_profile import response_bytes


def load(path):
    raw = Path(path).read_bytes()
    return json.loads(gzip.decompress(raw) if str(path).endswith(".gz") else raw)


def full_reply_bytes(response):
    """The public wire wraps the native response in another structuredContent."""
    response = deepcopy(response)
    if "structuredContent" in response:
        response["structuredContent"] = json.loads(response_bytes(response["structuredContent"]))
    return response_bytes(response)


def summarize(document):
    count = len(document["records"])
    cpu_ms = document["cpu_seconds_per_query"] * 1000
    phases = {}
    for name, phase in document["cpu_phase_ms"].items():
        total = phase["total"]
        calls = sum(sum(key == name for key, _ in trace["cpu_phases"])
                    for trace in document["traces"])
        phases[name] = {"cpu_ms_per_query": total / count,
                        "percent_of_daemon_cpu": total / count / cpu_ms * 100,
                        "calls_per_query": calls / count}
    return {"requests": count, "distinct_requests": len(document["queries"]),
            "cpu_ms_per_query": cpu_ms, "latency_ms": document["latency_ms"],
            "inclusive_cpu_phases": phases}


def compare(before, after):
    assert before["concurrency"] == after["concurrency"]
    for key in ("rerank_fixture", "watcher_disabled", "settle_replies"):
        assert before.get(key) == after.get(key), f"fixture changed: {key}"
    assert before["queries"] == after["queries"], "request population changed"
    assert len(before["queries"]) >= 100, "need at least 100 distinct requests"
    old = {row["id"]: row for row in before["records"]}
    new = {row["id"]: row for row in after["records"]}
    assert old.keys() == new.keys(), "capture population changed"
    assert len(old) >= 100, f"need at least 100 replies, got {len(old)}"
    digest = hashlib.sha256()
    response_field = "settled_full_response" if before.get("settle_replies") else "full_response"
    for identity in sorted(old):
        expected = full_reply_bytes(old[identity][response_field])
        actual = full_reply_bytes(new[identity][response_field])
        assert expected == actual, f"full reply bytes differ: {identity}"
        digest.update(len(expected).to_bytes(8, "little"))
        digest.update(expected)
    return {"concurrency": before["concurrency"], "identical_full_replies": len(old),
            "settled_replays": {"before": before.get("settled_replays", 0),
                                "after": after.get("settled_replays", 0)},
            "reply_sha256": digest.hexdigest(), "before": summarize(before),
            "after": summarize(after)}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before")
    parser.add_argument("after")
    args = parser.parse_args()
    print(json.dumps(compare(load(args.before), load(args.after)), indent=2))
