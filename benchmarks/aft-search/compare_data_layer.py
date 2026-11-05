#!/usr/bin/env python3
"""Replay aft_search requests twice on one tree and compare every reply.

One AFT process answers from today's indexes. A second runs with
`AFT_DATA_LAYER=1`, the development switch under which aft_search's lexical
and exact lanes read the data layer's view of the checkout once it has
published. Both processes get the same requests in the same order on the same
tree, each with its own empty storage, and every reply is compared whole:
text, results and structured content. Only fields that differ between any two
processes by construction are masked (`MASKED`); `--noise` runs today's path
a second time to show that those are the only ones.

The data-layer process serves from the view only after its first build has
published, so it is warmed with one request and the replay waits until the
build's log line appears. Each request then logs one line per ranking that
read the view, and every row records how many it saw, so a reply that came
from the legacy index anyway is visible rather than a silent pass.

Families:
  concept   the concept-recall fixtures (`fixtures.json`) on the pinned
            evidence tree, semantic lane served by the fixture vector pack;
  exact     the exact-recall fixtures on the provisioned corpus repositories,
            semantic search off, as `run_exact_recall.py` sends them;
  checkout  one query per line of `--queries` on `--root`, semantic search off.

Writes a JSON report (`--out`) and exits nonzero when any compared reply
differs or a data-layer reply did not read the view.
"""
from __future__ import annotations

import argparse
import difflib
import json
import os
import statistics
import sys
import tempfile
import time
from collections import ChainMap
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Iterator, Optional, Sequence

import bench_rerank
from run_real_query import (
    DEFAULT_BINARY,
    FIXTURE_PROVIDER_MODEL,
    AftProtocolError,
    NdjsonClient,
    ensure_binary,
    load_inputs,
    runtime_evidence_tree,
)
from search_quality_lib import PAGE_SIZE, InputFault, canonical_json

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
JsonObject = dict[str, Any]

# The ranking logs this line (target `aft::data_layer::tools::search`) each
# time its lanes read the data layer's view.
VIEW_READ = "read the data layer view"
# The data layer logs this line when a build has published.
BUILT = "data layer built"
# Fields that differ between two processes whatever they read: the snapshot
# generation carries a random per-process nonce.
MASKED = (("structuredContent", "plan", "snapshot_generation"),)
LOG_FILTER = "warn,aft::data_layer=debug"


def mask(response: JsonObject) -> JsonObject:
    """The reply without its request id and the per-process fields."""
    reply = json.loads(json.dumps(response))
    reply.pop("id", None)
    for path in MASKED:
        node: Any = reply
        for key in path[:-1]:
            node = node.get(key) if isinstance(node, dict) else None
        if isinstance(node, dict) and path[-1] in node:
            node[path[-1]] = "<masked>"
    return reply


class Process:
    """One AFT process reading either today's indexes or the data layer."""

    def __init__(self, binary: Path, root: Path, run_dir: Path, mode: str):
        self.mode = mode
        self.stderr_path = run_dir / f"{mode}.stderr"
        saved = {name: os.environ.get(name) for name in ("AFT_DATA_LAYER", "RUST_LOG")}
        try:
            if mode == "data":
                os.environ["AFT_DATA_LAYER"] = "1"
            else:
                os.environ.pop("AFT_DATA_LAYER", None)
            os.environ["RUST_LOG"] = LOG_FILTER
            self.client = NdjsonClient(binary, root, run_dir / f"{mode}-storage", self.stderr_path)
        finally:
            for name, value in saved.items():
                if value is None:
                    os.environ.pop(name, None)
                else:
                    os.environ[name] = value
        self.root = root

    def log(self) -> str:
        return self.stderr_path.read_text(encoding="utf-8", errors="replace")

    def view_reads(self) -> int:
        return self.log().count(VIEW_READ)

    def configure_without_semantic(self, timeout: float) -> None:
        doc = bench_rerank.apply_to_config(
            {"indexes": {"trigram": True, "semantic": False, "callgraph": False}}
        )
        response = self.client.call(
            "configure",
            {
                "project_root": str(self.root),
                "harness": "opencode",
                "storage_dir": str(self.client.storage_dir),
                "config": [
                    {"tier": "user", "source": "<aft-search-data-layer-parity>", "doc": json.dumps(doc)}
                ],
            },
            timeout,
        )
        if response.get("success") is not True:
            raise AftProtocolError(f"configure_failed:{response}")
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            status = self.client.call("status", timeout=30.0)
            search = status.get("search_index")
            if isinstance(search, dict) and search.get("status") == "ready":
                return
            time.sleep(0.2)
        raise AftProtocolError(f"index_ready_timeout:{self.client.stderr_text()}")

    def wait_for_data_layer(self, warm: JsonObject, send: Any, timeout: float) -> float:
        """Warm the data layer with one request and wait for its first build."""
        if self.mode != "data":
            return 0.0
        started = time.monotonic()
        send(self, warm)
        deadline = started + timeout
        while time.monotonic() < deadline:
            if BUILT in self.log():
                return time.monotonic() - started
            if "data layer build failed" in self.log() or "data layer could not bind" in self.log():
                raise AftProtocolError(f"data_layer_failed:{self.client.stderr_text()}")
            time.sleep(0.2)
        raise AftProtocolError(f"data_layer_build_timeout:{self.client.stderr_text()}")

    def close(self) -> None:
        self.client.close()


def send_search(process: Process, arguments: JsonObject) -> JsonObject:
    return process.client.call(
        "tool_call",
        {"session_id": "aft-search-data-layer-parity", "name": "search", "arguments": dict(arguments)},
    )


def send_semantic_search(process: Process, arguments: JsonObject) -> JsonObject:
    return process.client.call("semantic_search", dict(arguments), timeout=120.0)


def replay(process: Process, rows: Sequence[JsonObject], send: Any) -> list[JsonObject]:
    replies = []
    for row in rows:
        before = process.view_reads()
        started = time.perf_counter()
        response = send(process, row["request"])
        elapsed = (time.perf_counter() - started) * 1000.0
        replies.append(
            {
                "reply": mask(response),
                "latency_ms": elapsed,
                "view_reads": process.view_reads() - before,
            }
        )
    return replies


def compare(rows: Sequence[JsonObject], left: Sequence[JsonObject], right: Sequence[JsonObject]) -> JsonObject:
    differing = []
    for row, a, b in zip(rows, left, right):
        if a["reply"] == b["reply"]:
            continue
        a_text = json.dumps(a["reply"], indent=1, sort_keys=True).splitlines()
        b_text = json.dumps(b["reply"], indent=1, sort_keys=True).splitlines()
        diff = list(difflib.unified_diff(a_text, b_text, "legacy", "data", n=2, lineterm=""))
        differing.append({"id": row["id"], "request": row["request"], "diff": diff[:200]})
    return {"rows": len(rows), "differing": len(differing), "differences": differing}


def latency(replies: Sequence[JsonObject]) -> JsonObject:
    values = [reply["latency_ms"] for reply in replies]
    if not values:
        return {}
    return {
        "mean_ms": round(statistics.fmean(values), 3),
        "median_ms": round(statistics.median(values), 3),
        "max_ms": round(max(values), 3),
    }


def run_family(
    binary: Path,
    root: Path,
    rows: Sequence[JsonObject],
    send: Any,
    start: Any,
    noise: bool,
    timeout: float,
) -> JsonObject:
    modes = ["legacy", "data"] + (["legacy-again"] if noise else [])
    replies: dict[str, list[JsonObject]] = {}
    build_wait: dict[str, float] = {}
    with tempfile.TemporaryDirectory(prefix="aft-data-layer-parity-") as directory:
        for mode in modes:
            process = Process(binary, root, Path(directory), "data" if mode == "data" else mode)
            try:
                start(process)
                build_wait[mode] = process.wait_for_data_layer(rows[0]["request"], send, timeout)
                replies[mode] = replay(process, rows, send)
            finally:
                process.close()
    data_rows_without_view = [
        row["id"]
        for row, reply in zip(rows, replies["data"])
        if reply["view_reads"] == 0
    ]
    legacy_rows_with_view = [
        row["id"]
        for row, reply in zip(rows, replies["legacy"])
        if reply["view_reads"] != 0
    ]
    report: JsonObject = {
        "rows": len(rows),
        "legacy_vs_data": compare(rows, replies["legacy"], replies["data"]),
        "data_rows_without_a_view_read": data_rows_without_view,
        "legacy_rows_with_a_view_read": legacy_rows_with_view,
        "data_layer_first_build_seconds": round(build_wait.get("data", 0.0), 3),
        "latency": {mode: latency(replies[mode]) for mode in modes},
    }
    if noise:
        report["legacy_vs_legacy_again"] = compare(rows, replies["legacy"], replies["legacy-again"])
    return report


@contextmanager
def vector_server(vectors: Any, template: str, log: Path) -> Iterator[Any]:
    from run_concept_recall import vector_server as serve

    with serve(vectors, template, log) as server:
        yield server


def concept(args: argparse.Namespace) -> JsonObject:
    from run_concept_recall import INCLUDE_TESTS, check_query_pack, load_fixtures, require_query_vectors
    from search_quality_lib import sha256_file
    from vector_pack import read_pack

    fixtures = load_fixtures(HERE / "fixtures.json")
    _manifest, tree, corpus_pack_path, corpus_pack = load_inputs(HERE / "real-query-manifest.json")
    query_pack = read_pack(HERE / "concept-query-vectors.bin")
    check_query_pack(query_pack, corpus_pack, sha256_file(corpus_pack_path))
    template = str(corpus_pack["embed_template_version"])
    vectors = ChainMap(query_pack["vectors"], corpus_pack["vectors"])
    require_query_vectors(fixtures, vectors, template)
    rows = [
        {
            "id": f"concept:{index}",
            "request": {"query": fixture["query"], "topK": PAGE_SIZE, "includeTests": INCLUDE_TESTS},
        }
        for index, fixture in enumerate(fixtures)
    ]
    with tempfile.TemporaryDirectory(prefix="aft-data-layer-parity-vectors-") as log_dir, runtime_evidence_tree(tree) as root:
        with vector_server(vectors, template, Path(log_dir) / "requests.log") as server:
            endpoint = f"http://127.0.0.1:{server.server_port}"

            def start(process: Process) -> None:
                process.client.configure(endpoint, FIXTURE_PROVIDER_MODEL, args.ready_timeout)
                process.client.wait_ready(args.ready_timeout)

            report = run_family(args.binary, root, rows, send_search, start, args.noise, args.ready_timeout)
            report["vector_refusals"] = list(server.refused)
    return report


def exact(args: argparse.Namespace) -> JsonObject:
    from run_exact_recall import load_fixtures, validate_corpus
    from setup_corpus import parse_corpus_toml

    corpus_path = HERE / "corpus" / "corpus.toml"
    corpus, repos = parse_corpus_toml(corpus_path)
    clone_root = validate_corpus(corpus_path, corpus, repos)
    _count, fixtures = load_fixtures(HERE / "exact-recall-fixtures.json", [str(repo["name"]) for repo in repos])
    reports: JsonObject = {}
    for repo in repos:
        name = str(repo["name"])
        rows = [
            {
                "id": str(fixture["id"]),
                "request": {
                    "query": str(fixture["query"]),
                    "top_k": 5 if fixture["family"] == "sentence" else 10,
                },
            }
            for fixture in fixtures
            if fixture["repo"] == name
        ]

        def start(process: Process) -> None:
            process.configure_without_semantic(args.ready_timeout)

        reports[name] = run_family(
            args.binary, clone_root / name, rows, send_semantic_search, start, args.noise, args.ready_timeout
        )
    return reports


def checkout(args: argparse.Namespace) -> JsonObject:
    if args.root is None or args.queries is None:
        raise InputFault("checkout_family_needs:--root,--queries")
    queries: list[str] = []
    for line in Path(args.queries).read_text(encoding="utf-8").splitlines():
        query = line.strip()
        if query and query not in queries:
            queries.append(query)
    queries = queries[:: args.stride][: args.limit]
    rows = [{"id": f"checkout:{index}", "request": {"query": query}} for index, query in enumerate(queries)]

    def start(process: Process) -> None:
        process.configure_without_semantic(args.ready_timeout)

    return run_family(args.binary, Path(args.root).resolve(), rows, send_search, start, args.noise, args.ready_timeout)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    result.add_argument("family", choices=("concept", "exact", "checkout"))
    result.add_argument("--binary", default=DEFAULT_BINARY)
    result.add_argument("--out", required=True)
    result.add_argument("--root", help="checkout family: the tree to search")
    result.add_argument("--queries", help="checkout family: one query per line")
    result.add_argument("--stride", type=int, default=1, help="checkout family: replay every Nth query")
    result.add_argument("--limit", type=int, default=None, help="checkout family: replay at most N queries")
    result.add_argument("--noise", action="store_true", help="also replay today's path a second time")
    result.add_argument("--ready-timeout", type=float, default=900.0)
    return result


def main() -> int:
    args = parser().parse_args()
    args.binary = Path(args.binary).resolve()
    try:
        ensure_binary(args.binary)
        report = {"concept": concept, "exact": exact, "checkout": checkout}[args.family](args)
    except (AftProtocolError, InputFault, OSError, KeyError, ValueError) as error:
        print(f"{args.family}: {error}", file=sys.stderr)
        return 2
    output = Path(args.out)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(canonical_json({"family": args.family, "report": report}))
    families = report.values() if args.family == "exact" else [report]
    failed = False
    for family in families:
        compared = family["legacy_vs_data"]
        unread = family["data_rows_without_a_view_read"]
        print(
            f"{args.family}: {compared['differing']} of {compared['rows']} replies differ; "
            f"{len(unread)} data-layer replies read no view; latency {json.dumps(family['latency'])}"
        )
        if "legacy_vs_legacy_again" in family:
            print(f"{args.family}: noise {family['legacy_vs_legacy_again']['differing']} of {compared['rows']}")
        failed |= compared["differing"] > 0 or bool(unread) or bool(family["legacy_rows_with_a_view_read"])
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
