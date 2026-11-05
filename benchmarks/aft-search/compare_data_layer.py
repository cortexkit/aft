#!/usr/bin/env python3
"""Replay aft_search requests twice on one tree and compare every reply.

One AFT process answers from today's indexes. A second runs with
`AFT_DATA_LAYER=1`, the development switch under which aft_search's lexical
and exact lanes read the data layer's view of the checkout once it has
published, and its semantic lane reads the data layer's meaning plane, which
that process fills itself. Both processes get the same requests in the same
order on the same tree, each with its own empty storage, and every reply is
compared whole: text, results and structured content. Only fields that differ between any two
processes by construction are masked (`MASKED`). Two parts of a reply come
from outside aft_search and depend on timing: the status bar line the
response finaliser appends when background inspection counts change, and the
`watcher_pending` gap with its notice. A reply that differs only there is
counted as `process_noise`, apart from `differences`, and `--noise` runs
today's path a second time to show those parts move between two runs of the
same binary too.

One difference is allowed by name, `anchored_examined_count`, because the
new number is the true one: the anchored lane's "N of M candidate files"
disclosure and `anchored_admission_gap.candidates_examined` carry the data
layer's candidate count, which is far smaller than the legacy planner's
near-total count for a case-insensitive run. A reply is put in that class
only when everything else is identical, the excluded count included, and
each side's "shown" count is its examined count less the excluded ones; the
class lists every such reply with both counts.

Every process first waits until its indexes are ready (for the data-layer
process, its first build published and, with semantic search on, every file
filled), then answers the same warm-up request, which no row repeats. The
query embedding cache and the plan's `embedding_calls` and
`embedding_cache_hits` depend on what a process was asked before, so every
process must have been asked the same before the replay. Every process then
waits for the file watcher to go quiet before the replay.

The ranking logs one line each time its lanes read the data layer's view, and
one each time its semantic lane reads the meaning plane. Each replayed row
records how many such lines its request added to that process's log
(`view_reads`, `meaning_reads`), so a data-layer reply that the legacy index
answered anyway is reported rather than passing silently.

Replies are written to one file per process as they arrive and compared in a
stream afterwards; only the differing ones are kept, so a replay of the whole
real query list fits in memory.

Families:
  concept   the concept-recall fixtures (`fixtures.json`) on the evidence
            tree `provision_evidence.py` materializes (AFT at the commit
            `EVIDENCE_SHA` in `search_quality_lib.py` pins), semantic lane
            served by the fixture vector pack;
  exact     the exact-recall fixtures on the provisioned corpus repositories,
            semantic search off, as `run_exact_recall.py` sends them, or with
            `--semantic live` on, with the local model AFT ships by default;
  checkout  one query per line of `--queries` on `--root`, semantic search off,
            or with `--semantic hashed` on, served by a loopback embedder
            (`HashedEmbeddingServer`) that gives any text a deterministic
            vector, so both processes embed every chunk and query alike.

Writes a JSON report (`--out`) and exits nonzero when any compared reply
differs or a data-layer reply did not read the view.
"""
from __future__ import annotations

import argparse
import difflib
import hashlib
import json
import os
import re
import statistics
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from collections import ChainMap
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Iterable, Iterator, Optional, Sequence

import bench_rerank
from run_real_query import (
    DEFAULT_BINARY,
    FIXTURE_PROVIDER_MODEL,
    AftProtocolError,
    NdjsonClient,
    ensure_binary,
    live_model_env,
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
# The semantic lane logs this line each time it reads the meaning plane.
MEANING_READ = "read the data layer meaning plane"
# The data layer logs this line when a build has published.
BUILT = "data layer built"
# Fields that differ between two processes whatever they read: the snapshot
# generation carries a random per-process nonce.
MASKED = (("structuredContent", "plan", "snapshot_generation"),)
LOG_FILTER = "warn,aft::data_layer=debug"
# The local embedding model AFT ships by default (`--semantic live`).
DEFAULT_LOCAL_MODEL = "all-MiniLM-L6-v2"
# Sent first to every process of the semantic-off families; no row repeats it.
WARM_UP_QUERY = "data layer parity warm-up request"
STATUS_BAR = re.compile(r"\n\n\[AFT E[^\]\n]*\](?:\n|$)")
WATCHER_NOTICE = (
    "Watcher changes pending: indexed or cached results may omit unapplied file changes. "
    "Retry after maintenance catches up; a missing result is not evidence of absence."
)


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


ANCHORED_CUT = re.compile(r"Anchored verification cut short: (\d+) of (\d+) candidate files")
ANCHORED_SHOWN = re.compile(r"shown (\d+) of (\d+) candidate files verified")


def anchored_counts(reply: JsonObject) -> Optional[tuple[JsonObject, JsonObject]]:
    """The reply with its anchored examined count blanked, and the counts;
    `None` when it has no anchored disclosure or its counts disagree."""
    reply = json.loads(json.dumps(reply))
    text = reply.get("text")
    gap = reply.get("anchored_admission_gap")
    if not isinstance(text, str) or not isinstance(gap, dict):
        return None
    cut = ANCHORED_CUT.findall(text)
    shown = ANCHORED_SHOWN.findall(text)
    if len(cut) != 1 or len(shown) != 1:
        return None
    excluded, examined = (int(value) for value in cut[0])
    verified, total = (int(value) for value in shown[0])
    if total != examined or verified != examined - excluded:
        return None
    if gap.get("candidates_examined") != examined or gap.get("files_excluded") != excluded:
        return None
    text = ANCHORED_CUT.sub(f"Anchored verification cut short: {excluded} of <examined> candidate files", text)
    reply["text"] = ANCHORED_SHOWN.sub("shown <verified> of <examined> candidate files verified", text)
    gap["candidates_examined"] = "<examined>"
    return reply, {"examined": examined, "excluded": excluded}


def without_process_noise(left: JsonObject, right: JsonObject) -> tuple[JsonObject, JsonObject]:
    """Both replies without the status bar line and the watcher gap."""
    pair = [json.loads(json.dumps(left)), json.loads(json.dumps(right))]
    watcher = False
    for reply in pair:
        text = reply.get("text")
        if isinstance(text, str):
            text = STATUS_BAR.sub("", text)
            # The notice sits on its own line after a blank line, or ends the
            # text after one.
            text = re.sub(r"\n{1,2}" + re.escape(WATCHER_NOTICE), "", text)
            reply["text"] = text
        gaps = reply.get("gaps")
        if isinstance(gaps, list):
            kept = [gap for gap in gaps if not (isinstance(gap, dict) and gap.get("kind") == "watcher_pending")]
            watcher |= len(kept) != len(gaps)
            if kept:
                reply["gaps"] = kept
            else:
                reply.pop("gaps")
    if watcher:
        for reply in pair:
            reply.pop("complete", None)
    return pair[0], pair[1]


class Process:
    """One AFT process reading either today's indexes or the data layer."""

    def __init__(
        self, binary: Path, root: Path, run_dir: Path, mode: str, model_env: Optional[JsonObject] = None
    ):
        self.mode = mode
        self.stderr_path = run_dir / f"{mode}.stderr"
        saved = {name: os.environ.get(name) for name in ("AFT_DATA_LAYER", "RUST_LOG")}
        try:
            if mode == "data":
                os.environ["AFT_DATA_LAYER"] = "1"
            else:
                os.environ.pop("AFT_DATA_LAYER", None)
            os.environ["RUST_LOG"] = LOG_FILTER
            self.client = NdjsonClient(binary, root, run_dir / f"{mode}-storage", self.stderr_path, model_env)
        finally:
            for name, value in saved.items():
                if value is None:
                    os.environ.pop(name, None)
                else:
                    os.environ[name] = value
        self.root = root
        # Where `reads` stopped in the log, and its counts so far.
        self._log_offset = 0
        self._reads = {VIEW_READ: 0, MEANING_READ: 0}
        self._partial_line = ""

    def log(self) -> str:
        return self.stderr_path.read_text(encoding="utf-8", errors="replace")

    def reads(self) -> dict[str, int]:
        """How many view and meaning-plane reads the log holds so far. Reads
        only what was appended since the last call: the log grows with every
        request, and reading it whole per row is quadratic over a long
        replay."""
        with self.stderr_path.open("r", encoding="utf-8", errors="replace") as handle:
            handle.seek(self._log_offset)
            text = handle.read()
            self._log_offset = handle.tell()
        lines = (self._partial_line + text).split("\n")
        self._partial_line = lines.pop()
        for line in lines:
            for marker in self._reads:
                self._reads[marker] += line.count(marker)
        return dict(self._reads)

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

    def warm(self, warm: JsonObject, send: Any, timeout: float) -> float:
        """Answer the warm-up request; the data-layer process then waits for its
        first build to publish. Returns how long that build took to appear."""
        started = time.monotonic()
        send(self, warm)
        if self.mode != "data":
            return 0.0
        deadline = started + timeout
        while time.monotonic() < deadline:
            log = self.log()
            if BUILT in log:
                return time.monotonic() - started
            if "data layer build failed" in log or "data layer could not bind" in log:
                raise AftProtocolError(f"data_layer_failed:{self.client.stderr_text()}")
            time.sleep(0.2)
        raise AftProtocolError(f"data_layer_build_timeout:{self.client.stderr_text()}")

    def settle(self, timeout: float) -> None:
        """Wait until the watcher has seen no new event for three seconds."""
        deadline = time.monotonic() + timeout
        last: Any = None
        quiet_since = time.monotonic()
        while time.monotonic() < deadline:
            watcher = self.client.call("status", timeout=30.0).get("watcher")
            events = watcher.get("raw_events_total") if isinstance(watcher, dict) else None
            if events != last:
                last = events
                quiet_since = time.monotonic()
            elif time.monotonic() - quiet_since >= 3.0:
                return
            time.sleep(0.5)

    def close(self) -> None:
        self.client.close()


def send_search(process: Process, arguments: JsonObject) -> JsonObject:
    return process.client.call(
        "tool_call",
        {"session_id": "aft-search-data-layer-parity", "name": "search", "arguments": dict(arguments)},
    )


def send_semantic_search(process: Process, arguments: JsonObject) -> JsonObject:
    return process.client.call("semantic_search", dict(arguments), timeout=120.0)


def replay(process: Process, rows: Sequence[JsonObject], send: Any, out: Path) -> list[JsonObject]:
    """Sends every row, writing each masked reply to `out` (one JSON line per
    row) as it arrives; returns each row's latency and read counts."""
    measures = []
    with out.open("w", encoding="utf-8") as replies:
        for row in rows:
            before = process.reads()
            started = time.perf_counter()
            response = send(process, row["request"])
            elapsed = (time.perf_counter() - started) * 1000.0
            after = process.reads()
            replies.write(json.dumps(mask(response), sort_keys=True) + "\n")
            measures.append(
                {
                    "latency_ms": elapsed,
                    "view_reads": after[VIEW_READ] - before[VIEW_READ],
                    "meaning_reads": after[MEANING_READ] - before[MEANING_READ],
                    "semantic_lane": semantic_lane_ran(response),
                }
            )
    return measures


def semantic_lane_ran(response: JsonObject) -> bool:
    """Whether the reply's plan names the semantic lane among those it ran."""
    content = response.get("structuredContent")
    plan = content.get("plan") if isinstance(content, dict) else None
    lanes = plan.get("lanes_run") if isinstance(plan, dict) else None
    return isinstance(lanes, list) and "semantic" in lanes


def read_replies(path: Path) -> Iterator[JsonObject]:
    with path.open("r", encoding="utf-8") as replies:
        for line in replies:
            yield {"reply": json.loads(line)}


# Differing replies a comparison keeps whole; past this many only their ids
# are kept, so a systematic difference cannot fill memory.
KEPT_DIFFERENCES = 50


def compare(rows: Sequence[JsonObject], left: Iterable[JsonObject], right: Iterable[JsonObject]) -> JsonObject:
    differences = []
    differing_ids = []
    noise = []
    anchored = []
    for row, a, b in zip(rows, left, right):
        if a["reply"] == b["reply"]:
            continue
        a_clean, b_clean = without_process_noise(a["reply"], b["reply"])
        a_text = json.dumps(a["reply"], indent=1, sort_keys=True).splitlines()
        b_text = json.dumps(b["reply"], indent=1, sort_keys=True).splitlines()
        entry = {
            "id": row["id"],
            "request": row["request"],
            "diff": list(difflib.unified_diff(a_text, b_text, "first", "second", n=2, lineterm=""))[:200],
        }
        if a_clean == b_clean:
            noise.append({"id": row["id"], "diff": entry["diff"][:40]})
            continue
        a_counts, b_counts = anchored_counts(a_clean), anchored_counts(b_clean)
        if (
            a_counts is not None
            and b_counts is not None
            and a_counts[0] == b_counts[0]
            and a_counts[1]["excluded"] == b_counts[1]["excluded"]
        ):
            anchored.append(
                {
                    "id": row["id"],
                    "request": row["request"],
                    "excluded": a_counts[1]["excluded"],
                    "first_examined": a_counts[1]["examined"],
                    "second_examined": b_counts[1]["examined"],
                }
            )
            continue
        differing_ids.append(row["id"])
        if len(differences) < KEPT_DIFFERENCES:
            entry["first"] = a["reply"]
            entry["second"] = b["reply"]
            differences.append(entry)
    return {
        "rows": len(rows),
        "differing": len(differing_ids),
        "differing_ids": differing_ids,
        "process_noise": len(noise),
        "anchored_examined_count": len(anchored),
        "differences": differences,
        "noise": noise,
        "anchored_examined_count_rows": anchored,
    }


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
    warm: JsonObject,
    model_env: Optional[JsonObject] = None,
) -> JsonObject:
    modes = ["legacy", "data"] + (["legacy-again"] if noise else [])
    measures: dict[str, list[JsonObject]] = {}
    build_wait: dict[str, float] = {}
    ready_wait: dict[str, float] = {}
    with tempfile.TemporaryDirectory(prefix="aft-data-layer-parity-") as directory:
        replies = {mode: Path(directory) / f"{mode}-replies.jsonl" for mode in modes}
        for mode in modes:
            process = Process(binary, root, Path(directory), "data" if mode == "data" else mode, model_env)
            try:
                started = time.monotonic()
                start(process)
                ready_wait[mode] = time.monotonic() - started
                build_wait[mode] = process.warm(warm, send, timeout)
                process.settle(timeout)
                measures[mode] = replay(process, rows, send, replies[mode])
            finally:
                process.close()
        report: JsonObject = {
            "rows": len(rows),
            "legacy_vs_data": compare(rows, read_replies(replies["legacy"]), read_replies(replies["data"])),
        }
        if noise:
            report["legacy_vs_legacy_again"] = compare(
                rows, read_replies(replies["legacy"]), read_replies(replies["legacy-again"])
            )
    report.update(
        {
            "data_rows_without_a_view_read": [
                row["id"] for row, measure in zip(rows, measures["data"]) if measure["view_reads"] == 0
            ],
            "legacy_rows_with_a_view_read": [
                row["id"]
                for row, measure in zip(rows, measures["legacy"])
                if measure["view_reads"] != 0 or measure["meaning_reads"] != 0
            ],
            # A row whose data-layer reply ran the semantic lane without
            # reading the meaning plane read today's semantic index instead.
            "data_semantic_rows_without_a_meaning_read": [
                row["id"]
                for row, measure in zip(rows, measures["data"])
                if measure["semantic_lane"] and measure["meaning_reads"] == 0
            ],
            "semantic_lane_rows": {mode: sum(measure["semantic_lane"] for measure in measures[mode]) for mode in modes},
            "data_layer_first_build_seconds": round(build_wait.get("data", 0.0), 3),
            "ready_seconds": {mode: round(seconds, 3) for mode, seconds in ready_wait.items()},
            "latency": {mode: latency(measures[mode]) for mode in modes},
        }
    )
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

            warm = {"query": bench_rerank.burn_in_queries()[0], "topK": PAGE_SIZE, "includeTests": INCLUDE_TESTS}
            report = run_family(args.binary, root, rows, send_search, start, args.noise, args.ready_timeout, warm)
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
            if args.semantic == "live":
                process.client.configure(None, DEFAULT_LOCAL_MODEL, args.ready_timeout)
                process.client.wait_ready(args.ready_timeout)
            else:
                process.configure_without_semantic(args.ready_timeout)

        reports[name] = run_family(
            args.binary,
            clone_root / name,
            rows,
            send_semantic_search,
            start,
            args.noise,
            args.ready_timeout,
            {"query": WARM_UP_QUERY, "top_k": 5},
            live_model_env() if args.semantic == "live" else None,
        )
    return reports


class HashedEmbeddingServer(ThreadingHTTPServer):
    """A loopback OpenAI-compatible embedder that gives any text a
    deterministic vector: each word (identifiers split at case changes and
    underscores, lowercased) adds a signed unit to one of `DIMENSIONS`
    dimensions its digest picks. Texts sharing words point the same way, so
    the semantic lane ranks as a weak model would, and every process gets
    exactly the same vector for the same text."""

    DIMENSIONS = 384
    MODEL = "aft-search-hashed-v1"
    WORD = re.compile(r"[A-Z]+(?![a-z])|[A-Z]?[a-z0-9]+|[^\W_]+")

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), HashedEmbeddingHandler)
        self.texts = 0

    @classmethod
    def vector(cls, text: str) -> list[float]:
        vector = [0.0] * cls.DIMENSIONS
        for word in cls.WORD.findall(text):
            digest = hashlib.blake2b(word.lower().encode(), digest_size=4).digest()
            slot = int.from_bytes(digest[:2], "little") % cls.DIMENSIONS
            vector[slot] += 1.0 if digest[2] & 1 == 0 else -1.0
        return vector


class HashedEmbeddingHandler(BaseHTTPRequestHandler):
    def log_message(self, format: str, *args: Any) -> None:
        pass

    def do_POST(self) -> None:
        server = self.server
        if not self.path.endswith("/embeddings") or not isinstance(server, HashedEmbeddingServer):
            self.send_error(404)
            return
        payload = json.loads(self.rfile.read(int(self.headers.get("content-length", "0"))))
        texts = payload.get("input")
        texts = [texts] if isinstance(texts, str) else texts
        server.texts += len(texts)
        body = json.dumps(
            {
                "object": "list",
                "model": payload.get("model", HashedEmbeddingServer.MODEL),
                "data": [
                    {"object": "embedding", "index": index, "embedding": HashedEmbeddingServer.vector(text)}
                    for index, text in enumerate(texts)
                ],
            }
        ).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@contextmanager
def hashed_embedding_server() -> Iterator[HashedEmbeddingServer]:
    server = HashedEmbeddingServer()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def checkout(args: argparse.Namespace) -> JsonObject:
    if args.root is None or args.queries is None:
        raise InputFault("checkout_family_needs:--root,--queries")
    queries: list[str] = []
    seen: set[str] = set()
    for line in Path(args.queries).read_text(encoding="utf-8").splitlines():
        query = line.strip()
        if query and query not in seen:
            seen.add(query)
            queries.append(query)
    queries = queries[:: args.stride][: args.limit]
    rows = [{"id": f"checkout:{index}", "request": {"query": query}} for index, query in enumerate(queries)]
    root = Path(args.root).resolve()
    if args.semantic == "live":
        raise InputFault("checkout_family_semantic:live_is_for_the_exact_family")
    if args.semantic == "off":

        def start(process: Process) -> None:
            process.configure_without_semantic(args.ready_timeout)

        return run_family(
            args.binary, root, rows, send_search, start, args.noise, args.ready_timeout, {"query": WARM_UP_QUERY}
        )
    with hashed_embedding_server() as server:
        endpoint = f"http://127.0.0.1:{server.server_port}/v1"

        def start_semantic(process: Process) -> None:
            process.client.configure(endpoint, HashedEmbeddingServer.MODEL, args.ready_timeout)
            process.client.wait_ready(args.ready_timeout)

        report = run_family(
            args.binary,
            root,
            rows,
            send_search,
            start_semantic,
            args.noise,
            args.ready_timeout,
            {"query": WARM_UP_QUERY},
        )
        report["texts_embedded"] = server.texts
    return report


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    result.add_argument("family", choices=("concept", "exact", "checkout"))
    result.add_argument("--binary", default=DEFAULT_BINARY)
    result.add_argument("--out", required=True)
    result.add_argument("--root", help="checkout family: the tree to search")
    result.add_argument("--queries", help="checkout family: one query per line")
    result.add_argument("--stride", type=int, default=1, help="checkout family: replay every Nth query")
    result.add_argument("--limit", type=int, default=None, help="checkout family: replay at most N queries")
    result.add_argument(
        "--semantic",
        choices=("off", "hashed", "live"),
        default="off",
        help=(
            "checkout family: semantic search off, or on with the deterministic loopback embedder (hashed); "
            "exact family: off, or on with the local model AFT ships by default (live; its ONNX Runtime and "
            "files named by ORT_DYLIB_PATH and FASTEMBED_CACHE_DIR, never downloaded mid-run)"
        ),
    )
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
            f"{args.family}: {compared['differing']} of {compared['rows']} replies differ, "
            f"{compared['process_noise']} only in process noise, "
            f"{compared['anchored_examined_count']} only in the anchored examined count; "
            f"{len(unread)} data-layer replies read no view; latency {json.dumps(family['latency'])}"
        )
        if "legacy_vs_legacy_again" in family:
            again = family["legacy_vs_legacy_again"]
            print(
                f"{args.family}: today's path twice: {again['differing']} differ, "
                f"{again['process_noise']} only in process noise"
            )
        unread_meaning = family["data_semantic_rows_without_a_meaning_read"]
        if unread_meaning:
            print(f"{args.family}: {len(unread_meaning)} data-layer replies ran the semantic lane without the meaning plane")
        print(f"{args.family}: semantic lane rows {json.dumps(family['semantic_lane_rows'])}")
        failed |= (
            compared["differing"] > 0
            or bool(unread)
            or bool(unread_meaning)
            or bool(family["legacy_rows_with_a_view_read"])
        )
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
