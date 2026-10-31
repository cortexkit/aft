#!/usr/bin/env python3
"""Compare complete responses and CPU costs from the Linux engine-only profile test."""
from __future__ import annotations

import argparse
from collections import Counter
import gzip
import json
from pathlib import Path
import statistics


def load(path: Path) -> dict:
    data = gzip.decompress(path.read_bytes()) if path.suffix == ".gz" else path.read_bytes()
    return json.loads(data)


def summary(document: dict) -> dict:
    result = {"files": document["files"], "modes": {}}
    for mode in sorted({sample["mode"] for sample in document["samples"]}):
        samples = [sample for sample in document["samples"] if sample["mode"] == mode]
        elapsed = sorted(sample["elapsed_ms"] for sample in samples)
        work = Counter()
        for sample in samples:
            work.update(sample["work"])
        result["modes"][mode] = {
            "queries": len(samples),
            "cpu_ms_per_query": statistics.mean(sample["cpu_ms"] for sample in samples),
            "p50_ms": statistics.median(elapsed),
            "p95_ms": elapsed[int(0.95 * (len(elapsed) - 1))],
            "work_per_query": {key: value / len(samples) for key, value in work.items()},
        }
    return result


def phases(path: Path) -> dict:
    data = gzip.decompress(path.read_bytes()) if path.suffix == ".gz" else path.read_bytes()
    cpu, calls = Counter(), Counter()
    requests = 0
    for line in data.decode().splitlines():
        if line.startswith("AFT_SEARCH_COST "):
            request = json.loads(line.removeprefix("AFT_SEARCH_COST "))
            requests += 1
            for name, value in request["cpu_phases"]:
                cpu[name] += value
                calls[name] += 1
    assert requests > 0, "no feature-gated CPU traces found"
    return {"requests": requests, "inclusive_cpu_ms_per_query": {
        name: {"ms": value / requests, "calls": calls[name] / requests}
        for name, value in cpu.most_common()
    }}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path, nargs="?")
    parser.add_argument("--baseline-log", type=Path)
    parser.add_argument("--candidate-log", type=Path)
    args = parser.parse_args()
    before = load(args.baseline)
    result = {"baseline": summary(before)}
    if args.baseline_log:
        result["baseline_phases"] = phases(args.baseline_log)
    if args.candidate:
        after = load(args.candidate)
        assert before["files"] == after["files"], "corpus file count changed"
        key = lambda sample: (sample["mode"], sample["number"])
        old = {key(sample): sample for sample in before["samples"]}
        new = {key(sample): sample for sample in after["samples"]}
        assert len(old) == len(before["samples"]) and len(new) == len(after["samples"]), "duplicate requests"
        assert old.keys() == new.keys(), "request population changed"
        for identity, sample in old.items():
            candidate = new[identity]
            assert sample["episode_id"] == candidate["episode_id"], f"input changed: {identity}"
            assert sample["response"] == candidate["response"], f"response changed: {identity}, {sample['episode_id']}"
            assert sample["response_blake3"] == candidate["response_blake3"], f"response bytes changed: {identity}"
        result["byte_identical_responses"] = len(old)
        result["candidate"] = summary(after)
    if args.candidate_log:
        result["candidate_phases"] = phases(args.candidate_log)
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
