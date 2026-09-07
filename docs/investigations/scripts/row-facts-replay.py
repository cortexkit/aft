#!/usr/bin/env python3
"""Replay 500 historical agent queries on AFT, a TS repo, and a Python repo.

Search indexes are built in temporary directories. Candidate facts are joined
from the existing callgraph/inspect SQLite generations through mode=ro handles.
No OpenCode or AFT cache database is modified.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import random
import select
import shutil
import subprocess
import sys
import tempfile
import time
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any

SCRIPT_DIR = Path(__file__).resolve().parent
CORE_PATH = SCRIPT_DIR / "row-facts-spike.py"
SPEC = importlib.util.spec_from_file_location("row_facts_spike", CORE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot load {CORE_PATH}")
CORE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = CORE
SPEC.loader.exec_module(CORE)

TARGETS = {
    "aft": (Path.home() / "Work/Projects/CortexKit/aft", Path.home() / "Work/Projects/CortexKit/aft"),
    "opencode-ts": (Path.home() / "Work/OSS/opencode", Path.home() / "Work/OSS/opencode"),
    # MTPLX has no recent session, so its replay corpus is sampled from the full
    # real-query pool rather than inventing symbol-name queries for the target.
    "mtplx-python": (Path.home() / "Work/OSS/MTPLX", None),
}
TOTAL_SEARCHES = 500
SEED = 20_260_909


class AftClient:
    def __init__(self, binary: Path, root: Path, storage: Path) -> None:
        self.process = subprocess.Popen(
            [str(binary)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            bufsize=0,
        )
        self.root = root
        self.storage = storage
        self.buffer = b""
        self.request_id = 0

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)

    def call(self, command: str, timeout: float = 120, **params: Any) -> dict[str, Any]:
        self.request_id += 1
        request_id = str(self.request_id)
        request = {"id": request_id, "command": command, **params}
        if self.process.stdin is None or self.process.stdout is None:
            raise RuntimeError("AFT protocol pipes are unavailable")
        self.process.stdin.write((json.dumps(request, separators=(",", ":")) + "\n").encode())
        self.process.stdin.flush()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"AFT exited {self.process.returncode}")
            readable, _, _ = select.select([self.process.stdout], [], [], 0.1)
            if readable:
                chunk = os.read(self.process.stdout.fileno(), 65536)
                if chunk:
                    self.buffer += chunk
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                try:
                    frame = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if frame.get("id") == request_id:
                    return frame
        raise TimeoutError(f"AFT command {command} timed out")

    def configure(self) -> None:
        response = self.call(
            "configure",
            project_root=str(self.root),
            harness="opencode",
            storage_dir=str(self.storage),
            config=[{
                "tier": "user",
                "source": "<row-facts-replay>",
                "doc": json.dumps({
                    "search_index": True,
                    "semantic_search": True,
                    "callgraph_store": False,
                }),
            }],
        )
        if not response.get("success"):
            raise RuntimeError(f"configure failed: {response}")
        deadline = time.monotonic() + 600
        last = {}
        while time.monotonic() < deadline:
            last = self.call("status", timeout=120)
            search = last.get("search_index", {})
            semantic = last.get("semantic_index", {})
            if (
                isinstance(search, dict) and search.get("status") == "ready"
                and isinstance(semantic, dict) and semantic.get("status") == "ready"
            ):
                return
            time.sleep(0.25)
        raise TimeoutError(f"search index did not become ready: {last}")


def real_queries(db: Any, start_ms: int, end_ms: int, repository: Path | None) -> list[str]:
    sql = """
    SELECT json_extract(p.data,'$.state.input.query') AS query
    FROM part p
    JOIN session s ON s.id=p.session_id
    JOIN project pr ON pr.id=s.project_id
    WHERE p.time_created BETWEEN ? AND ?
      AND json_extract(p.data,'$.tool')='aft_search'
      AND json_extract(p.data,'$.state.status')='completed'
      AND json_extract(p.data,'$.state.input.query') IS NOT NULL
    """
    params: list[Any] = [start_ms, end_ms]
    if repository is not None:
        sql += " AND pr.worktree=?"
        params.append(str(repository))
    return [str(row[0]) for row in db.execute(sql, params) if str(row[0]).strip()]


def result_rows(response: dict[str, Any], root: Path) -> list[Any]:
    output = []
    for result in response.get("results", []):
        if not isinstance(result, dict):
            continue
        path = result.get("file")
        symbol = result.get("name")
        if not isinstance(path, str) or not isinstance(symbol, str) or not symbol:
            continue
        start = result.get("start_line")
        end = result.get("end_line")
        output.append(CORE.ResultRow(CORE.normalize_path(str(Path(path).relative_to(root))), symbol, int(start) if start is not None else None, int(end) if end is not None else None))
    return output


def seed_storage(source: Path, destination: Path, root: Path) -> None:
    cache_keys = json.loads((source / "cache-keys.json").read_text())
    record = cache_keys[str(root.resolve())]
    key = record["key"]
    destination.mkdir(parents=True, exist_ok=True)
    (destination / "cache-keys.json").write_text(json.dumps({str(root.resolve()): record}))
    for plane in ("index", "semantic"):
        source_path = source / plane / key
        destination_parent = destination / plane
        destination_parent.mkdir(parents=True, exist_ok=True)
        # APFS clone-copy keeps the replay isolated without physically duplicating
        # nearly a gigabyte of immutable search artifacts.
        subprocess.run(["cp", "-cR", str(source_path), str(destination_parent / key)], check=True)
    for shared in ("models", "onnxruntime"):
        source_path = source / shared
        if source_path.exists():
            (destination / shared).symlink_to(source_path, target_is_directory=True)


def summarize(values: list[int]) -> dict[str, float | int]:
    if not values:
        return {"mean": 0, "median": 0, "p95": 0, "total": 0}
    values = sorted(values)
    middle = len(values) // 2
    median = values[middle] if len(values) % 2 else (values[middle - 1] + values[middle]) / 2
    return {
        "mean": round(sum(values) / len(values), 2),
        "median": median,
        "p95": values[max(0, (95 * len(values) + 99) // 100 - 1)],
        "total": sum(values),
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--db", type=Path, default=CORE.DEFAULT_DB)
    parser.add_argument("--aft-storage", type=Path, default=CORE.DEFAULT_AFT_STORAGE)
    parser.add_argument("--end-ms", type=int, required=True)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    rng = random.Random(SEED)
    db = CORE.open_ro_sqlite(args.db)
    quotas = {label: TOTAL_SEARCHES // len(TARGETS) for label in TARGETS}
    for label in list(TARGETS)[: TOTAL_SEARCHES % len(TARGETS)]:
        quotas[label] += 1
    output: dict[str, Any] = {
        "seed": SEED,
        "end_ms": args.end_ms,
        "searches": TOTAL_SEARCHES,
        "top_k": 10,
        "targets": {},
    }
    try:
        for label, (root, query_repository) in TARGETS.items():
            queries = real_queries(db, args.end_ms - 30 * CORE.DAY_MS, args.end_ms, query_repository)
            chosen = rng.sample(queries, quotas[label])
            stores = CORE.ReadOnlyStores(root, args.aft_storage)
            temporary = Path(tempfile.mkdtemp(prefix=f"row-facts-{label}-"))
            seed_storage(args.aft_storage, temporary, root)
            client = AftClient(args.binary.resolve(), root.resolve(), temporary)
            per_search: dict[str, list[int]] = defaultdict(list)
            coverage = Counter()
            row_count = 0
            try:
                client.configure()
                for query in chosen:
                    response = client.call("semantic_search", query=query, top_k=10)
                    if not response.get("success"):
                        raise RuntimeError(f"search failed for {label}: {response}")
                    rows = result_rows(response, root.resolve())[:10]
                    row_count += len(rows)
                    totals = Counter()
                    for row in rows:
                        for fact, suffix in stores.facts_for(row).suffixes().items():
                            totals[fact] += len(suffix.encode())
                            coverage[fact] += int(bool(suffix))
                    for fact in (
                        "direct_callers", "test_files", "complexity", "edge_provenance",
                        "bundle", "deduplicated_bundle",
                    ):
                        per_search[fact].append(totals[fact])
            finally:
                client.close()
                stores.close()
                shutil.rmtree(temporary)
            output["targets"][label] = {
                "root": str(root),
                "query_source": str(query_repository) if query_repository else "all recent real aft_search queries",
                "searches": len(chosen),
                "rows": row_count,
                "added_bytes_per_search": {
                    fact: summarize(values) for fact, values in sorted(per_search.items())
                },
                "rows_with_fact": dict(sorted(coverage.items())),
            }
    finally:
        db.close()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(output, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
