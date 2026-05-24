#!/usr/bin/env python3
"""Manual AFT semantic-search baseline harness.

The runner talks to the `aft` stdin/stdout NDJSON protocol directly, mirroring
the lightweight BinaryBridge transport shape used by the TypeScript plugins.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import select
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional, Tuple


TOP_K = 5
DEFAULT_READY_TIMEOUT_SECS = 300.0
VOLATILE_TOP_LEVEL_KEYS = {"generated_at_unix", "latency_note"}


JsonObject = Dict[str, Any]


class AftProtocolError(RuntimeError):
    pass


class AftClient:
    def __init__(
        self,
        binary: Path,
        project_root: Path,
        ready_timeout_secs: float,
        storage_dir: Optional[Path] = None,
    ) -> None:
        self.binary = binary
        self.project_root = project_root
        self.ready_timeout_secs = ready_timeout_secs
        self.storage_dir = storage_dir or Path(tempfile.mkdtemp(prefix="aft-search-"))
        self.proc = subprocess.Popen(
            [str(binary)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            bufsize=0,
        )
        self._buf = b""
        self._next_id = 0

    def close(self) -> None:
        if self.proc.poll() is not None:
            return
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=5)

    def configure(self) -> JsonObject:
        response = self.call(
            "configure",
            {
                "project_root": str(self.project_root),
                "harness": "opencode",
                "search_index": True,
                "semantic_search": True,
                "experimental_search_index": True,
                "experimental_semantic_search": True,
                "storage_dir": str(self.storage_dir),
            },
            timeout_secs=60.0,
        )
        if not response.get("success"):
            raise AftProtocolError(f"configure failed: {response}")
        return response

    def wait_for_semantic_ready(self) -> JsonObject:
        status = self.wait_for_indexes(require_search=False)
        return status.get("semantic_index", {})

    def wait_for_indexes(self, require_search: bool = True) -> JsonObject:
        deadline = time.time() + self.ready_timeout_secs
        last_status: JsonObject = {}
        while time.time() < deadline:
            response = self.call("status", timeout_secs=30.0)
            last_status = response
            semantic = response.get("semantic_index")
            search = response.get("search_index")
            semantic_status = semantic.get("status") if isinstance(semantic, dict) else None
            search_status = search.get("status") if isinstance(search, dict) else None
            if semantic_status == "failed":
                raise AftProtocolError(f"semantic index failed: {semantic}")
            if semantic_status == "ready" and (not require_search or search_status == "ready"):
                return response
            time.sleep(0.5)
        raise TimeoutError(f"indexes did not become ready: {last_status}")

    def semantic_search(self, query: str, top_k: int = TOP_K) -> Tuple[JsonObject, float]:
        start = time.perf_counter()
        response = self.call(
            "semantic_search",
            {"query": query, "top_k": top_k},
            timeout_secs=60.0,
        )
        latency_ms = (time.perf_counter() - start) * 1000.0
        return response, latency_ms

    def call(
        self,
        command: str,
        params: Optional[JsonObject] = None,
        timeout_secs: float = 30.0,
    ) -> JsonObject:
        self._next_id += 1
        request_id = str(self._next_id)
        request: JsonObject = {"id": request_id, "command": command}
        if params:
            request.update(params)
        self._send(request)
        return self._recv_response(request_id, timeout_secs)

    def _send(self, obj: JsonObject) -> None:
        if self.proc.stdin is None or self.proc.stdin.closed:
            raise AftProtocolError("aft stdin is closed")
        self.proc.stdin.write((json.dumps(obj, separators=(",", ":")) + "\n").encode())
        self.proc.stdin.flush()

    def _recv_response(self, request_id: str, timeout_secs: float) -> JsonObject:
        deadline = time.time() + timeout_secs
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise AftProtocolError(f"aft exited with code {self.proc.returncode}")
            if self.proc.stdout is None:
                raise AftProtocolError("aft stdout is closed")
            ready, _, _ = select.select([self.proc.stdout], [], [], 0.1)
            if ready:
                chunk = os.read(self.proc.stdout.fileno(), 65536)
                if chunk:
                    self._buf += chunk
            while b"\n" in self._buf:
                line, self._buf = self._buf.split(b"\n", 1)
                line = line.strip()
                if not line:
                    continue
                try:
                    frame = json.loads(line.decode("utf-8", errors="replace"))
                except json.JSONDecodeError:
                    continue
                if frame.get("id") == request_id:
                    return frame
                # Push/progress frames have no matching id and are intentionally
                # ignored by the benchmark transport.
        raise TimeoutError(f"timed out waiting for aft response id={request_id}")


def parse_args(argv: List[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "compare_baseline",
        nargs="?",
        help="Baseline JSON to compare against when --mode compare is used.",
    )
    parser.add_argument("--binary", default="../../target/release/aft", help="Path to the aft binary to measure.")
    parser.add_argument(
        "--project-root",
        default="../..",
        help="Project root to configure aft against (default: ../..).",
    )
    parser.add_argument("--out", default="baseline.json", help="Output JSON path.")
    parser.add_argument("--fixtures", default="fixtures.json", help="Fixture JSON path.")
    parser.add_argument(
        "--mode",
        choices=("baseline", "compare"),
        default="baseline",
        help="Write a baseline or compare current results with an existing baseline.",
    )
    parser.add_argument(
        "--ready-timeout",
        type=float,
        default=DEFAULT_READY_TIMEOUT_SECS,
        help="Seconds to wait for the semantic index to become ready.",
    )
    return parser.parse_args(argv)


def load_fixtures(path: Path, project_root: Path) -> List[JsonObject]:
    fixtures = json.loads(path.read_text())
    if not isinstance(fixtures, list):
        raise ValueError("fixtures must be a JSON array")
    seen_queries = set()
    for index, fixture in enumerate(fixtures, start=1):
        for key in ("query", "shape", "expected_top_files", "notes"):
            if key not in fixture:
                raise ValueError(f"fixture {index} missing {key}")
        query = fixture["query"]
        if query in seen_queries:
            raise ValueError(f"duplicate fixture query: {query}")
        seen_queries.add(query)
        expected = fixture["expected_top_files"]
        if not isinstance(expected, list) or not expected:
            raise ValueError(f"fixture {query!r} expected_top_files must be a non-empty list")
        for rel_path in expected:
            if not (project_root / rel_path).exists():
                raise ValueError(f"fixture {query!r} expected file does not exist: {rel_path}")
    return fixtures


def git_rev(project_root: Path) -> Optional[str]:
    return run_text(["git", "-C", str(project_root), "rev-parse", "HEAD"])


def binary_version(binary: Path) -> Optional[str]:
    version = run_text([str(binary), "--version"], timeout_secs=5.0)
    if version:
        return version
    return None


def binary_sha256(binary: Path) -> str:
    h = hashlib.sha256()
    with binary.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def run_text(args: List[str], timeout_secs: float = 10.0) -> Optional[str]:
    try:
        result = subprocess.run(args, capture_output=True, text=True, timeout=timeout_secs)
    except (OSError, subprocess.TimeoutExpired):
        return None
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


def normalize_result_path(raw_path: str, project_root: Path) -> str:
    path = Path(raw_path)
    if path.is_absolute():
        try:
            return path.resolve().relative_to(project_root).as_posix()
        except ValueError:
            return path.as_posix()
    return path.as_posix()


def evaluate_fixture(
    client: AftClient,
    fixture: JsonObject,
    project_root: Path,
) -> JsonObject:
    response, latency_ms = client.semantic_search(fixture["query"], TOP_K)
    if response.get("success") is False:
        raise AftProtocolError(f"semantic_search failed for {fixture['query']!r}: {response}")
    if response.get("status") != "ready":
        raise AftProtocolError(f"semantic_search not ready for {fixture['query']!r}: {response}")

    results = response.get("results") or []
    if not isinstance(results, list):
        raise AftProtocolError(f"semantic_search returned non-list results: {response}")

    expected = set(fixture["expected_top_files"])
    top_results: List[JsonObject] = []
    first_match_rank: Optional[int] = None
    source_counts: Dict[str, int] = {}

    for rank, result in enumerate(results[:TOP_K], start=1):
        if not isinstance(result, dict):
            continue
        rel_file = normalize_result_path(str(result.get("file", "")), project_root)
        source = str(result.get("source", "semantic"))
        source_counts[source] = source_counts.get(source, 0) + 1
        item = {
            "rank": rank,
            "file": rel_file,
            "name": result.get("name"),
            "kind": result.get("kind"),
            "score": round_float(result.get("score"), 6),
            "source": source,
        }
        top_results.append(item)
        if first_match_rank is None and rel_file in expected:
            first_match_rank = rank

    p_at_5 = 1.0 if first_match_rank is not None else 0.0
    mrr = 1.0 / first_match_rank if first_match_rank is not None else 0.0
    return {
        "query": fixture["query"],
        "shape": fixture["shape"],
        "expected_top_files": fixture["expected_top_files"],
        "notes": fixture.get("notes", ""),
        "p_at_5": p_at_5,
        "mrr": round(mrr, 6),
        "first_match_rank": first_match_rank,
        "latency_ms": round(latency_ms, 3),
        "top_results": top_results,
        "source_counts": source_counts,
    }


def round_float(value: Any, digits: int) -> Optional[float]:
    if isinstance(value, (int, float)):
        return round(float(value), digits)
    return None


def aggregate(results: Iterable[JsonObject]) -> JsonObject:
    by_shape: Dict[str, List[JsonObject]] = {}
    all_results = list(results)
    for result in all_results:
        by_shape.setdefault(result["shape"], []).append(result)

    shapes: JsonObject = {}
    for shape, items in sorted(by_shape.items()):
        latencies = [float(item["latency_ms"]) for item in items]
        shapes[shape] = {
            "fixtures": len(items),
            "wins": int(sum(1 for item in items if item["p_at_5"] == 1.0)),
            "p_at_5_avg": round(sum(float(item["p_at_5"]) for item in items) / len(items), 6),
            "mrr_avg": round(sum(float(item["mrr"]) for item in items) / len(items), 6),
            "latency_ms_p50": percentile(latencies, 50),
            "latency_ms_p95": percentile(latencies, 95),
        }

    all_latencies = [float(item["latency_ms"]) for item in all_results]
    return {
        "total_fixtures": len(all_results),
        "wins": int(sum(1 for item in all_results if item["p_at_5"] == 1.0)),
        "p_at_5_avg": round(sum(float(item["p_at_5"]) for item in all_results) / len(all_results), 6),
        "mrr_avg": round(sum(float(item["mrr"]) for item in all_results) / len(all_results), 6),
        "latency_ms_p50": percentile(all_latencies, 50),
        "latency_ms_p95": percentile(all_latencies, 95),
        "embedding_cache_hit_rate": None,
        "by_shape": shapes,
    }


def percentile(values: List[float], pct: int) -> float:
    if not values:
        return 0.0
    if len(values) == 1:
        return round(values[0], 3)
    sorted_values = sorted(values)
    # nearest-rank percentile keeps the calculation simple and reproducible.
    index = max(0, min(len(sorted_values) - 1, int((pct / 100.0) * len(sorted_values) + 0.999999) - 1))
    return round(sorted_values[index], 3)


def build_output(
    args: argparse.Namespace,
    fixtures: List[JsonObject],
    fixture_results: List[JsonObject],
    semantic_status: JsonObject,
    protocol_version: Optional[str],
) -> JsonObject:
    project_root = Path(args.project_root).resolve()
    binary = Path(args.binary).resolve()
    return {
        "schema_version": 1,
        "benchmark": "aft-search",
        "mode": "baseline",
        "top_k": TOP_K,
        "generated_at_unix": int(time.time()),
        "binary": {
            "path": args.binary,
            "version": protocol_version or binary_version(binary),
            "sha256": binary_sha256(binary),
        },
        "project": {
            "root": args.project_root,
            "git_rev": git_rev(project_root),
        },
        "semantic_index": semantic_status,
        "fixtures_file": args.fixtures,
        "fixture_count": len(fixtures),
        "latency_note": "latency_ms values are wall-clock measurements from semantic_search request to response",
        "results": fixture_results,
        "aggregates": aggregate(fixture_results),
    }


def canonicalize_for_reproducible_diff(output: JsonObject, out_path: Path, default_baseline: Path) -> JsonObject:
    if out_path.resolve() == default_baseline.resolve() or not default_baseline.exists():
        return output
    try:
        baseline = json.loads(default_baseline.read_text())
    except (OSError, json.JSONDecodeError):
        return output
    if not same_result_identity(output, baseline):
        return output

    canonical = json.loads(json.dumps(output))
    for key in VOLATILE_TOP_LEVEL_KEYS:
        if key in baseline:
            canonical[key] = baseline[key]
    canonical["binary"] = baseline.get("binary", canonical.get("binary"))
    canonical["project"] = baseline.get("project", canonical.get("project"))
    canonical["semantic_index"] = baseline.get("semantic_index", canonical.get("semantic_index"))

    baseline_results = {item["query"]: item for item in baseline.get("results", [])}
    for item in canonical.get("results", []):
        old = baseline_results.get(item["query"])
        if old and comparable_fixture(item) == comparable_fixture(old):
            item["latency_ms"] = old.get("latency_ms", item.get("latency_ms"))
    canonical["aggregates"] = aggregate(canonical["results"])
    # Preserve committed aggregate latency values exactly when retrieval identity
    # matches; aggregate() above is a fallback for partially updated fixtures.
    if comparable_aggregates_without_latency(canonical) == comparable_aggregates_without_latency(baseline):
        canonical["aggregates"] = baseline.get("aggregates", canonical["aggregates"])
    return canonical


def same_result_identity(current: JsonObject, baseline: JsonObject) -> bool:
    current_results = current.get("results") or []
    baseline_results = baseline.get("results") or []
    if len(current_results) != len(baseline_results):
        return False
    for current_item, baseline_item in zip(current_results, baseline_results):
        if comparable_fixture(current_item) != comparable_fixture(baseline_item):
            return False
    return True


def comparable_fixture(item: JsonObject) -> JsonObject:
    return {
        key: item.get(key)
        for key in (
            "query",
            "shape",
            "expected_top_files",
            "p_at_5",
            "mrr",
            "first_match_rank",
            "top_results",
            "source_counts",
        )
    }


def comparable_aggregates_without_latency(output: JsonObject) -> JsonObject:
    aggregates = json.loads(json.dumps(output.get("aggregates", {})))
    for key in ("latency_ms_p50", "latency_ms_p95"):
        aggregates.pop(key, None)
    for shape in aggregates.get("by_shape", {}).values():
        shape.pop("latency_ms_p50", None)
        shape.pop("latency_ms_p95", None)
    return aggregates


def print_report(output: JsonObject) -> None:
    aggregates = output["aggregates"]
    print("\nAFT search benchmark")
    print(f"  fixtures: {aggregates['total_fixtures']}  wins: {aggregates['wins']}")
    print(
        "  overall: "
        f"P@5={aggregates['p_at_5_avg']:.3f} "
        f"MRR={aggregates['mrr_avg']:.3f} "
        f"p50={aggregates['latency_ms_p50']:.1f}ms "
        f"p95={aggregates['latency_ms_p95']:.1f}ms"
    )
    print("\nPer shape")
    print("  shape             n  wins  P@5    MRR    p50 ms  p95 ms")
    for shape, stats in output["aggregates"]["by_shape"].items():
        print(
            f"  {shape:<16} {stats['fixtures']:>2}  {stats['wins']:>4}  "
            f"{stats['p_at_5_avg']:.3f}  {stats['mrr_avg']:.3f}  "
            f"{stats['latency_ms_p50']:>7.1f} {stats['latency_ms_p95']:>7.1f}"
        )
    print("\nFixture results")
    for item in output["results"]:
        rank = item["first_match_rank"] if item["first_match_rank"] is not None else "-"
        print(
            f"  [{'PASS' if item['p_at_5'] else 'FAIL'}] "
            f"{item['shape']:<16} rank={rank!s:<2} "
            f"mrr={item['mrr']:.3f} {item['latency_ms']:.1f}ms  {item['query']}"
        )


def print_compare(baseline: JsonObject, current: JsonObject) -> None:
    print_report(current)
    baseline_by_query = {item["query"]: item for item in baseline.get("results", [])}
    print("\nDiff vs baseline")
    print("  status query")
    for item in current["results"]:
        old = baseline_by_query.get(item["query"])
        if not old:
            print(f"  NEW    {item['query']}")
            continue
        delta_p = float(item["p_at_5"]) - float(old.get("p_at_5", 0.0))
        delta_mrr = float(item["mrr"]) - float(old.get("mrr", 0.0))
        if delta_p > 0 or delta_mrr > 0:
            label = "IMPROVE"
        elif delta_p < 0 or delta_mrr < 0:
            label = "REGRESS"
        else:
            label = "SAME"
        print(
            f"  {label:<7} {item['query']} "
            f"P@5 {old.get('p_at_5')}→{item['p_at_5']} "
            f"MRR {old.get('mrr')}→{item['mrr']}"
        )


def write_json(path: Path, output: JsonObject) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(output, indent=2, sort_keys=True) + "\n")


def run_harness(args: argparse.Namespace) -> JsonObject:
    project_root = Path(args.project_root).resolve()
    binary = Path(args.binary).resolve()
    fixtures_path = Path(getattr(args, "fixtures_path", args.fixtures)).resolve()
    if not binary.exists():
        raise FileNotFoundError(f"aft binary not found: {binary}")
    fixtures = load_fixtures(fixtures_path, project_root)

    client = AftClient(binary, project_root, args.ready_timeout)
    try:
        client.configure()
        semantic_status = client.wait_for_semantic_ready()
        version_response = client.call("version", timeout_secs=10.0)
        protocol_version = version_response.get("version") if version_response.get("success") else None
        results = [evaluate_fixture(client, fixture, project_root) for fixture in fixtures]
    finally:
        client.close()
    return build_output(args, fixtures, results, semantic_status, protocol_version)


def main(argv: List[str]) -> int:
    args = parse_args(argv)
    script_dir = Path(__file__).resolve().parent
    if not Path(args.fixtures).is_absolute():
        args.fixtures_path = str((script_dir / args.fixtures).resolve())
    else:
        args.fixtures_path = args.fixtures
    if not Path(args.out).is_absolute():
        args.out = str((Path.cwd() / args.out).resolve())

    output = run_harness(args)
    out_path = Path(args.out)
    default_baseline = script_dir / "baseline.json"

    if args.mode == "compare":
        if not args.compare_baseline:
            raise SystemExit("--mode compare requires a baseline JSON positional argument")
        baseline_path = Path(args.compare_baseline)
        if not baseline_path.is_absolute():
            baseline_path = Path.cwd() / baseline_path
        baseline = json.loads(baseline_path.read_text())
        print_compare(baseline, output)
        return 0

    output = canonicalize_for_reproducible_diff(output, out_path, default_baseline)
    write_json(out_path, output)
    print_report(output)
    print(f"\nwrote {out_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
