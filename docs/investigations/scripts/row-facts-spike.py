#!/usr/bin/env python3
"""Reproduce the row-facts follow-up census, audit sample, and byte model.

The OpenCode database and AFT artifact stores are always opened through SQLite
URI mode=ro. This program writes only the paths explicitly passed to --output
and --sample-csv.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
import random
import re
import sqlite3
import statistics
from collections import Counter, defaultdict, deque
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable, Iterator, Sequence
from urllib.parse import quote

DAY_MS = 86_400_000
DEFAULT_DB = Path.home() / ".local/share/opencode/opencode.db"
DEFAULT_AFT_STORAGE = Path.home() / ".local/share/cortexkit/aft"
SEED = 20_260_907
SAMPLE_SIZE = 200
TOKEN_SEARCHES = 500

REPOSITORIES = {
    "aft": Path.home() / "Work/Projects/CortexKit/aft",
    "magic-context": Path.home() / "Work/Projects/CortexKit/magic-context",
    "prefrontal": Path.home() / "Work/Projects/CortexKit/prefrontal",
}
STORE_SHAPE_REPOSITORIES = {
    "aft": Path.home() / "Work/Projects/CortexKit/aft",
    "opencode-ts": Path.home() / "Work/OSS/opencode",
    "mtplx-python": Path.home() / "Work/OSS/MTPLX",
}

TERMINAL_TOOL_CALLS_SQL = """
SELECT
  p.id, p.message_id, p.session_id, p.time_created,
  s.directory, pr.worktree AS repository,
  json_extract(p.data, '$.tool') AS tool,
  json_extract(p.data, '$.state.status') AS status,
  json_extract(p.data, '$.state.input') AS input_json,
  json_extract(p.data, '$.state.output') AS output_text,
  json_extract(p.data, '$.state.error') AS error_json
FROM part AS p
JOIN session AS s ON s.id = p.session_id
JOIN project AS pr ON pr.id = s.project_id
WHERE p.time_created BETWEEN :start_ms AND :end_ms
  AND json_extract(p.data, '$.tool') IS NOT NULL
  AND json_extract(p.data, '$.state.status') IN ('completed', 'error')
ORDER BY p.session_id, p.time_created, p.id
"""

VOLUME_SQL = """
SELECT pr.worktree AS repository,
       COUNT(*) AS completed_searches,
       COUNT(DISTINCT p.session_id) AS sessions
FROM part AS p
JOIN session AS s ON s.id = p.session_id
JOIN project AS pr ON pr.id = s.project_id
WHERE p.time_created BETWEEN :start_ms AND :end_ms
  AND json_extract(p.data, '$.tool') = 'aft_search'
  AND json_extract(p.data, '$.state.status') = 'completed'
GROUP BY pr.worktree
ORDER BY completed_searches DESC
"""

PATH_ROW_RE = re.compile(r"^(?P<path>\S.*?\.[A-Za-z0-9_+-]+)(?: \[lexical match\])?$")
SYMBOL_ROW_RE = re.compile(
    r"^  (?P<symbol>.+?) \[[^]]+\](?: lines? (?P<start>\d+)(?:-(?P<end>\d+))?)?"
)
CALLGRAPH_PATH_RE = re.compile(r"^(?P<path>\S.*?\.[A-Za-z0-9_+-]+)(?::\d+)?$")
CALLGRAPH_SYMBOL_RE = re.compile(
    r"^  ↳ (?P<symbol>.+?)(?: \[entry\])?(?: ~)?(?::\d+)?$"
)
TEST_INTENT_RE = re.compile(r"\b(test|tests|tested|coverage|assert|spec)\b", re.I)
SMOKE_SYMBOL_RE = re.compile(r"(^|::|\.)(<top-level>|setup|setUp|smoke|import|fixture|helper|snapshot)$", re.I)


@dataclass(frozen=True)
class ResultRow:
    path: str
    symbol: str | None = None
    start_line: int | None = None
    end_line: int | None = None

    @property
    def end_line_or_start(self) -> int:
        return self.end_line or self.start_line or 0


@dataclass
class ToolCall:
    id: str
    message_id: str
    session_id: str
    time_created: int
    directory: str
    repository: str
    tool: str
    status: str
    input: dict[str, Any]
    input_text: str
    output: str
    error_text: str


@dataclass
class Event:
    anchor: ToolCall
    followup: ToolCall
    ordinal: int
    category: str
    rows: list[ResultRow] = field(repr=False)


@dataclass
class RowFacts:
    callers: int | None = None
    test_files: tuple[str, ...] = ()
    provenance: tuple[tuple[str, int], ...] = ()
    complexity: int | None = None
    ambiguous: bool = False
    smoke_test_files: tuple[str, ...] = ()

    def suffixes(self) -> dict[str, str]:
        caller = "" if self.callers is None else f" callers={self.callers}"
        tests = ""
        if self.test_files:
            shown = list(self.test_files[:2])
            extra = len(self.test_files) - len(shown)
            value = ",".join(shown) + (f",+{extra}" if extra else "")
            tests = f" tests={value}"
        complexity = "" if self.complexity is None else f" cx={self.complexity}"
        provenance = ""
        if self.provenance:
            labels = {"treesitter+resolver": "r", "type_match": "t", "name_match": "n"}
            value = "/".join(f"{labels.get(name, name)}{count}" for name, count in self.provenance)
            provenance = f" edges={value}"
        return {
            "direct_callers": caller,
            "test_files": tests,
            "complexity": complexity,
            "edge_provenance": provenance,
            "bundle": caller + tests + complexity + provenance,
            # Search output already includes the caller-count marker (`↩N`), so this
            # bundle contains only the additional test, complexity, and provenance facts.
            "deduplicated_bundle": tests + complexity + provenance,
        }


class ReadOnlyStores:
    def __init__(self, root: Path, storage: Path) -> None:
        self.root = root.resolve()
        self.storage = storage
        self.callgraph: sqlite3.Connection | None = None
        self.inspect: sqlite3.Connection | None = None
        self.reverse: dict[tuple[str, str], list[tuple[str, str, str, str]]] = defaultdict(list)
        self.nodes_by_file: dict[str, list[sqlite3.Row]] = defaultdict(list)
        self.complexity_by_file: dict[str, tuple[os.stat_result, list[dict[str, Any]]]] = {}
        self._open_callgraph()
        self._open_inspect()

    def close(self) -> None:
        if self.callgraph is not None:
            self.callgraph.close()
        if self.inspect is not None:
            self.inspect.close()

    def _open_callgraph(self) -> None:
        cache_keys_path = self.storage / "cache-keys.json"
        if not cache_keys_path.exists():
            return
        cache_keys = json.loads(cache_keys_path.read_text())
        record = cache_keys.get(str(self.root))
        if not record:
            return
        key = record["key"]
        directory = self.storage / "callgraph" / key
        pointer = directory / f"{key}.current"
        if not pointer.exists():
            return
        target = directory / pointer.read_text().strip()
        if not target.exists():
            return
        self.callgraph = open_ro_sqlite(target)
        for row in self.callgraph.execute(
            "SELECT file_path,name,scoped_name,start_line,end_line FROM nodes ORDER BY file_path,start_line"
        ):
            self.nodes_by_file[row["file_path"]].append(row)
        for row in self.callgraph.execute(
            """SELECT e.target_file,e.target_symbol,s.file_path,s.scoped_name,e.provenance,e.ref_id
               FROM edges e JOIN nodes s ON s.id=e.source_node
               WHERE e.kind='call'"""
        ):
            key_tuple = (row["target_file"], row["target_symbol"])
            self.reverse[key_tuple].append(
                (row["file_path"], row["scoped_name"], row["provenance"], row["ref_id"])
            )

    def _open_inspect(self) -> None:
        key = hashlib.sha256(str(self.root).encode()).hexdigest()[:16]
        directory = self.storage / "inspect" / key
        pointer = directory / f"{key}.current"
        if not pointer.exists():
            return
        target = directory / pointer.read_text().strip()
        if not target.exists():
            return
        self.inspect = open_ro_sqlite(target)
        rows = self.inspect.execute(
            """SELECT file_path,file_mtime_ns,file_size,contribution
               FROM tier2_contributions
               WHERE category='complexity' AND project_key=?""",
            (key,),
        )
        for row in rows:
            path = self.root / row["file_path"]
            try:
                stat = path.stat()
            except OSError:
                continue
            # This is AFT's hot-fresh check. Metadata misses are omitted rather than
            # synchronously hashing files, preserving the candidate's "fresh only" rule.
            if stat.st_mtime_ns != row["file_mtime_ns"] or stat.st_size != row["file_size"]:
                continue
            try:
                payload = json.loads(bytes(row["contribution"]).decode())
            except (UnicodeDecodeError, json.JSONDecodeError):
                continue
            self.complexity_by_file[row["file_path"]] = (stat, payload.get("functions", []))

    def facts_for(self, row: ResultRow) -> RowFacts:
        target = self._resolve_target(row)
        if target is None:
            return RowFacts()
        if target == ("", ""):
            return RowFacts(ambiguous=True)
        incoming = self.reverse.get(target, [])
        provenance = Counter(edge[2] for edge in incoming)
        ordered_provenance = tuple(
            (name, provenance[name])
            for name in ("treesitter+resolver", "type_match", "name_match")
            if provenance[name]
        )
        complexity, complexity_ambiguous = self._complexity(row)
        test_files, smoke_test_files = self._test_origins(target)
        return RowFacts(
            callers=len({edge[3] for edge in incoming}),
            test_files=tuple(sorted(test_files)),
            provenance=ordered_provenance,
            complexity=complexity,
            ambiguous=complexity_ambiguous,
            smoke_test_files=tuple(sorted(smoke_test_files)),
        )

    def _resolve_target(self, row: ResultRow) -> tuple[str, str] | None:
        if not row.symbol or self.callgraph is None:
            return None
        candidates = []
        for node in self.nodes_by_file.get(row.path, []):
            scoped = node["scoped_name"]
            name = node["name"]
            if not symbol_matches(row.symbol, name, scoped):
                continue
            if row.start_line is not None and not (
                int(node["start_line"]) <= row.end_line_or_start <= int(node["end_line"])
                or row.start_line <= int(node["start_line"]) <= row.end_line_or_start
            ):
                continue
            candidates.append(scoped)
        candidates = sorted(set(candidates))
        if not candidates and (row.path, row.symbol) in self.reverse:
            return (row.path, row.symbol)
        if len(candidates) != 1:
            return ("", "") if candidates else None
        return (row.path, candidates[0])

    def _test_origins(self, target: tuple[str, str]) -> tuple[set[str], set[str]]:
        origins: set[str] = set()
        smoke_origins: set[str] = set()
        queue: deque[tuple[str, str]] = deque([target])
        visited = {target}
        while queue:
            current = queue.popleft()
            for source_file, source_symbol, _provenance, _ref_id in self.reverse.get(current, []):
                source = (source_file, source_symbol)
                if is_test_file(source_file):
                    origins.add(source_file)
                    if SMOKE_SYMBOL_RE.search(source_symbol):
                        smoke_origins.add(source_file)
                elif source not in visited:
                    visited.add(source)
                    queue.append(source)
        return origins, smoke_origins

    def _complexity(self, row: ResultRow) -> tuple[int | None, bool]:
        if not row.symbol:
            return None, False
        cached = self.complexity_by_file.get(row.path)
        if cached is None:
            return None, False
        _stat, functions = cached
        matches = [
            function
            for function in functions
            if symbol_matches(row.symbol, str(function.get("function", "")), str(function.get("function", "")))
        ]
        in_range = [
            function for function in matches
            if row.start_line is not None
            and row.start_line <= int(function.get("line", 0)) <= row.end_line_or_start
        ]
        candidates = in_range or matches
        if len(candidates) == 1:
            return int(candidates[0].get("complexity", 0)), False
        return None, len(candidates) > 1


def open_ro_sqlite(path: Path) -> sqlite3.Connection:
    uri = f"file:{quote(str(path.resolve()), safe='/')}?mode=ro"
    connection = sqlite3.connect(uri, uri=True)
    connection.row_factory = sqlite3.Row
    connection.execute("PRAGMA query_only=ON")
    return connection


def parse_json_object(value: str | None) -> dict[str, Any]:
    if not value:
        return {}
    try:
        parsed = json.loads(value)
    except json.JSONDecodeError:
        return {}
    return parsed if isinstance(parsed, dict) else {}


def load_calls(db: sqlite3.Connection, start_ms: int, end_ms: int) -> list[ToolCall]:
    calls = []
    for row in db.execute(
        TERMINAL_TOOL_CALLS_SQL, {"start_ms": start_ms, "end_ms": end_ms}
    ):
        calls.append(
            ToolCall(
                id=row["id"],
                message_id=row["message_id"],
                session_id=row["session_id"],
                time_created=row["time_created"],
                directory=row["directory"],
                repository=row["repository"],
                tool=row["tool"],
                status=row["status"],
                input=parse_json_object(row["input_json"]),
                input_text=row["input_json"] or "",
                output=row["output_text"] or "",
                error_text=row["error_json"] or "",
            )
        )
    return calls


def parse_rows(tool: str, output: str) -> list[ResultRow]:
    rows: list[ResultRow] = []
    current_path: str | None = None
    for line in output.splitlines():
        path_match = (CALLGRAPH_PATH_RE if tool == "aft_callgraph" else PATH_ROW_RE).match(line)
        if path_match and not line.startswith(("Found ", "No ", "Error", "Incomplete ", "Zoom ")):
            current_path = normalize_path(path_match.group("path"))
            rows.append(ResultRow(current_path))
            continue
        if current_path is None:
            continue
        symbol_match = (CALLGRAPH_SYMBOL_RE if tool == "aft_callgraph" else SYMBOL_ROW_RE).match(line)
        if not symbol_match:
            continue
        symbol = clean_symbol(symbol_match.group("symbol"))
        start = symbol_match.groupdict().get("start")
        end = symbol_match.groupdict().get("end")
        rows.append(
            ResultRow(
                current_path,
                symbol,
                int(start) if start else None,
                int(end) if end else (int(start) if start else None),
            )
        )
    return rows


def clean_symbol(symbol: str) -> str:
    return re.sub(r"\s+(?:↩\d+.*|lines? \d+.*)$", "", symbol).strip()


def normalize_path(path: str) -> str:
    return path.replace("\\", "/").removeprefix("./")


def symbol_matches(query: str, name: str, scoped: str) -> bool:
    if query in {name, scoped}:
        return True
    query_tail = re.split(r"::|\.|#", query)[-1]
    name_tail = re.split(r"::|\.|#", name)[-1]
    scoped_tail = re.split(r"::|\.|#", scoped)[-1]
    return query_tail == name_tail == scoped_tail


def is_test_file(path: str) -> bool:
    normalized = normalize_path(path)
    if any(part in {"__tests__", "__test__", "tests"} for part in normalized.split("/")):
        return True
    filename = normalized.rsplit("/", 1)[-1]
    lower = filename.lower()
    if ".test." in lower or ".spec." in lower:
        return True
    if lower.endswith(("_test.rs", "_test.go", "_test.py", "_test.rb", "_test.exs", "_spec.rb")):
        return True
    if lower.startswith("test_") and lower.endswith(".py"):
        return True
    return filename.endswith(
        (
            "Test.java", "Tests.java", "Test.kt", "Tests.kt", "Test.cs", "Tests.cs",
            "Test.swift", "Tests.swift", "Test.scala", "Tests.scala", "Spec.scala",
        )
    )


def row_pairs(rows: Sequence[ResultRow]) -> set[tuple[str, str]]:
    return {(row.path, row.symbol) for row in rows if row.symbol}


def row_paths(rows: Sequence[ResultRow]) -> set[str]:
    return {row.path for row in rows}


def path_relative_to_repo(path: str, call: ToolCall) -> str:
    normalized = normalize_path(path)
    for root in (call.directory, call.repository):
        root_norm = normalize_path(root).rstrip("/")
        if normalized == root_norm:
            return "."
        if normalized.startswith(root_norm + "/"):
            return normalized[len(root_norm) + 1 :]
    return normalized


def zoom_targets(call: ToolCall) -> Iterator[tuple[str, str]]:
    targets = call.input.get("targets")
    if isinstance(targets, dict):
        targets = [targets]
    if isinstance(targets, list):
        for target in targets:
            if isinstance(target, dict) and isinstance(target.get("path"), str):
                symbol = target.get("symbol")
                if isinstance(symbol, str):
                    yield path_relative_to_repo(target["path"], call), symbol
        return
    path = call.input.get("path")
    symbols = call.input.get("symbols")
    if isinstance(symbols, str):
        symbols = [symbols]
    if isinstance(path, str) and isinstance(symbols, list):
        for symbol in symbols:
            if isinstance(symbol, str):
                yield path_relative_to_repo(path, call), symbol


def classify_followup(anchor: ToolCall, followup: ToolCall, rows: Sequence[ResultRow]) -> str:
    pairs = row_pairs(rows)
    paths = row_paths(rows)
    if followup.tool == "aft_zoom":
        if any((path, symbol) in pairs for path, symbol in zoom_targets(followup)):
            return "a_zoom_returned_symbol"
        return "g_unrelated"
    if followup.tool == "aft_callgraph" and followup.input.get("op") in {"callers", "impact"}:
        path = followup.input.get("path")
        symbol = followup.input.get("symbol")
        if isinstance(path, str) and isinstance(symbol, str):
            pair = (path_relative_to_repo(path, followup), symbol)
            if pair in pairs:
                return "b_callgraph_returned_symbol"
        return "g_unrelated"
    if followup.tool == "aft_inspect":
        scope = followup.input.get("scope")
        scopes = scope if isinstance(scope, list) else [scope]
        roots = {None, ".", "", anchor.directory, anchor.repository}
        if any(item in roots for item in scopes):
            return "c_inspect_root"
        return "g_unrelated"
    if followup.tool == "read":
        path = followup.input.get("filePath", followup.input.get("path"))
        if isinstance(path, str) and path_relative_to_repo(path, followup) in paths:
            return "d_read_returned_file"
        return "g_unrelated"
    if followup.tool == "edit":
        path = followup.input.get("filePath", followup.input.get("path"))
        if isinstance(path, str) and path_relative_to_repo(path, followup) in paths:
            return "e_edit_returned_file"
        return "g_unrelated"
    if followup.tool == "aft_search":
        return "f_search_refinement"
    return "g_unrelated"


def build_events(calls: Sequence[ToolCall], selected_repositories: set[str]) -> tuple[list[ToolCall], list[Event]]:
    sessions: dict[str, list[ToolCall]] = defaultdict(list)
    for call in calls:
        sessions[call.session_id].append(call)
    anchors: list[ToolCall] = []
    events: list[Event] = []
    for session_calls in sessions.values():
        session_calls.sort(key=lambda call: (call.time_created, call.id))
        for index, anchor in enumerate(session_calls):
            if anchor.repository not in selected_repositories or anchor.status != "completed" or not anchor.output:
                continue
            if anchor.tool == "aft_search":
                pass
            elif anchor.tool == "aft_callgraph" and anchor.input.get("op") in {"callers", "impact"}:
                pass
            else:
                continue
            anchors.append(anchor)
            rows = parse_rows(anchor.tool, anchor.output)
            followups = [
                call
                for call in session_calls[index + 1 :]
                if call.message_id != anchor.message_id
            ][:5]
            for ordinal, followup in enumerate(followups, 1):
                events.append(
                    Event(
                        anchor=anchor,
                        followup=followup,
                        ordinal=ordinal,
                        category=classify_followup(anchor, followup, rows),
                        rows=rows,
                    )
                )
    return anchors, events


def summarize_census(anchors: Sequence[ToolCall], events: Sequence[Event]) -> dict[str, Any]:
    output: dict[str, Any] = {}
    by_key: dict[tuple[str, str], list[Event]] = defaultdict(list)
    anchor_counts = Counter()
    for anchor in anchors:
        anchor_kind = anchor.tool
        if anchor.tool == "aft_callgraph":
            anchor_kind += ":" + str(anchor.input.get("op"))
        anchor_counts[(repo_label(anchor.repository), anchor_kind)] += 1
    for event in events:
        anchor_kind = event.anchor.tool
        if event.anchor.tool == "aft_callgraph":
            anchor_kind += ":" + str(event.anchor.input.get("op"))
        by_key[(repo_label(event.anchor.repository), anchor_kind)].append(event)
    for key in sorted(anchor_counts):
        repo, anchor_kind = key
        bucket = by_key[key]
        counts = Counter(event.category for event in bucket)
        windows = len({event.anchor.id for event in bucket})
        incidence = {
            category: len({event.anchor.id for event in bucket if event.category == category})
            for category in counts
        }
        output[f"{repo}/{anchor_kind}"] = {
            "anchors": anchor_counts[key],
            "anchors_with_a_followup": windows,
            "classified_followup_calls": len(bucket),
            "call_distribution": {
                category: {"count": count, "pct": round(100 * count / len(bucket), 2) if bucket else 0}
                for category, count in sorted(counts.items())
            },
            "anchor_window_incidence": {
                category: {
                    "anchors": count,
                    "pct": round(100 * count / anchor_counts[key], 2) if anchor_counts[key] else 0,
                }
                for category, count in sorted(incidence.items())
            },
            "removable_ceiling_abc_calls": sum(counts[name] for name in (
                "a_zoom_returned_symbol", "b_callgraph_returned_symbol", "c_inspect_root"
            )),
            "removable_ceiling_abc_anchor_windows": len({
                event.anchor.id for event in bucket
                if event.category in {
                    "a_zoom_returned_symbol", "b_callgraph_returned_symbol", "c_inspect_root"
                }
            }),
        }
    return output


def target_row(event: Event) -> ResultRow | None:
    if event.category == "a_zoom_returned_symbol":
        targets = set(zoom_targets(event.followup))
    elif event.category == "b_callgraph_returned_symbol":
        path = event.followup.input.get("path")
        symbol = event.followup.input.get("symbol")
        targets = {
            (path_relative_to_repo(path, event.followup), symbol)
        } if isinstance(path, str) and isinstance(symbol, str) else set()
    else:
        return None
    return next((row for row in event.rows if (row.path, row.symbol) in targets), None)


def test_paths_from_output(output: str) -> list[str]:
    return [row.path for row in parse_rows("aft_callgraph", output) if row.symbol is None and is_test_file(row.path)]


def rate_event(event: Event, facts: RowFacts | None) -> tuple[dict[str, str], dict[str, bool], dict[str, str]]:
    ratings = {name: "no" for name in (
        "direct_callers", "test_files", "complexity", "edge_provenance"
    )}
    misleading = {name: False for name in ratings}
    reasons = {name: "does not answer this follow-up" for name in ratings}

    context = json.dumps(event.anchor.input, sort_keys=True) + " " + event.followup.input_text
    if facts is not None:
        provenance = dict(facts.provenance)
        exact = provenance.get("treesitter+resolver", 0) + provenance.get("type_match", 0)
        approximate = provenance.get("name_match", 0)
        if facts.callers and approximate > exact:
            misleading["direct_callers"] = True
            reasons["direct_callers"] += "; name_match call sites outnumber exact/type-matched sites"

    if event.category == "b_callgraph_returned_symbol":
        op = str(event.followup.input.get("op"))
        first_line = event.followup.output.splitlines()[0] if event.followup.output else ""
        match = re.match(r"(\d+) callers?\b", first_line)
        if op == "callers" and match and int(match.group(1)) == 0:
            ratings["direct_callers"] = "yes"
            reasons["direct_callers"] = "the callers response contained only the zero count"
        elif op == "callers" and match:
            ratings["direct_callers"] = "partial"
            reasons["direct_callers"] = "the count helps, but caller identities and sites were requested"
        elif op == "impact":
            reasons["direct_callers"] = "direct fan-in does not answer transitive impact"

        test_paths = test_paths_from_output(event.followup.output)
        known_test_paths = sorted(set(test_paths) | set(facts.test_files if facts is not None else ()))
        if known_test_paths:
            if test_paths and TEST_INTENT_RE.search(context) and op == "callers" and set(test_paths) == set(
                row.path for row in parse_rows("aft_callgraph", event.followup.output) if row.symbol is None
            ):
                ratings["test_files"] = "yes"
                reasons["test_files"] = "test intent and an all-test caller result needed file identities only"
            else:
                ratings["test_files"] = "partial"
                reasons["test_files"] = "test file identities omit the caller symbol, line, and assertion semantics"
        else:
            reasons["test_files"] = "the follow-up did not return test-origin callers"

        if re.search(r"^  ↳ .+ ~$", event.followup.output, re.MULTILINE):
            ratings["edge_provenance"] = "partial"
            reasons["edge_provenance"] = "approximation counts help, but not which rendered caller edge is approximate"
        else:
            reasons["edge_provenance"] = "the follow-up did not expose an approximate edge decision"

        if facts is not None and any(name == "name_match" and count for name, count in facts.provenance):
            if ratings["direct_callers"] != "no":
                misleading["direct_callers"] = True
                reasons["direct_callers"] += "; aggregate count includes name_match call sites"
        symbols = [row.symbol or "" for row in parse_rows("aft_callgraph", event.followup.output)]
        smoke_output = any(SMOKE_SYMBOL_RE.search(symbol) for symbol in symbols)
        if ratings["test_files"] != "no" and (
            smoke_output or (facts is not None and facts.smoke_test_files)
        ):
            misleading["test_files"] = True
            reasons["test_files"] += "; a top-level/setup/smoke-style test reach is not a behavior assertion"

    elif event.category == "c_inspect_root":
        sections = event.followup.input.get("sections")
        section_list = [sections] if isinstance(sections, str) else sections if isinstance(sections, list) else []
        if section_list == ["complexity"]:
            ratings["complexity"] = "partial"
            reasons["complexity"] = "row complexity helps for returned symbols but not the root-wide hotspot list/freshness gate"
        else:
            reasons["complexity"] = "a per-row score does not replace a root health/diagnostics inspection"

    return ratings, misleading, reasons


def build_sample(
    events: Sequence[Event], stores: dict[str, ReadOnlyStores]
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    eligible = [
        event for event in events
        if event.category in {"a_zoom_returned_symbol", "b_callgraph_returned_symbol", "c_inspect_root"}
        and event.followup.output
    ]
    rng = random.Random(SEED)
    # The classes row facts could plausibly answer are rare. Audit every eligible
    # callgraph/inspect event, then randomly fill the remaining slots with zooms.
    rare = [event for event in eligible if event.category != "a_zoom_returned_symbol"]
    zooms = [event for event in eligible if event.category == "a_zoom_returned_symbol"]
    if len(rare) >= SAMPLE_SIZE:
        sample = rng.sample(rare, SAMPLE_SIZE)
    else:
        sample = rare + rng.sample(zooms, min(SAMPLE_SIZE - len(rare), len(zooms)))
        rng.shuffle(sample)
    records = []
    tallies = {
        fact: {"yes": 0, "no": 0, "partial": 0, "misleading": 0}
        for fact in ("direct_callers", "test_files", "complexity", "edge_provenance")
    }
    replacement_bytes = Counter()
    fact_available = Counter()
    for event in sample:
        row = target_row(event)
        store = stores.get(event.anchor.repository)
        facts = store.facts_for(row) if store is not None and row is not None else None
        ratings, misleading, reasons = rate_event(event, facts)
        if facts is not None:
            fact_available["direct_callers"] += int(facts.callers is not None)
            fact_available["test_files"] += int(bool(facts.test_files))
            fact_available["complexity"] += int(facts.complexity is not None)
            fact_available["edge_provenance"] += int(bool(facts.provenance))
        for fact, rating in ratings.items():
            tallies[fact][rating] += 1
            tallies[fact]["misleading"] += int(misleading[fact])
            if rating == "yes":
                replacement_bytes[fact] += len(event.followup.input_text.encode()) + len(event.followup.output.encode())
        record = {
            "anchor_id": event.anchor.id,
            "followup_id": event.followup.id,
            "repository": repo_label(event.anchor.repository),
            "category": event.category,
            "ordinal": event.ordinal,
            "anchor_query": event.anchor.input.get("query", ""),
            "followup_tool": event.followup.tool,
            "followup_input": event.followup.input_text,
            "followup_output_bytes": len(event.followup.output.encode()),
            "target": f"{row.path}::{row.symbol}" if row else "<root>",
            "rendered_facts": json.dumps(asdict(facts), sort_keys=True) if facts is not None else "{}",
        }
        for fact in tallies:
            record[f"{fact}_rating"] = ratings[fact]
            record[f"{fact}_misleading"] = misleading[fact]
            record[f"{fact}_reason"] = reasons[fact]
        records.append(record)
    summary = {
        "population": len(eligible),
        "sample_size": len(sample),
        "seed": SEED,
        "sample_by_category": dict(sorted(Counter(record["category"] for record in records).items())),
        "sample_by_repository": dict(sorted(Counter(record["repository"] for record in records).items())),
        "fact_tallies": tallies,
        "fact_available": dict(fact_available),
        "mislead_rate_when_available": {
            fact: round(100 * tally["misleading"] / fact_available[fact], 2)
            if fact_available[fact] else 0
            for fact, tally in tallies.items()
        },
        "replaceable_followup_input_plus_output_bytes": dict(replacement_bytes),
    }
    return records, summary


def quantiles(values: Sequence[int]) -> dict[str, float]:
    if not values:
        return {"mean": 0, "median": 0, "p95": 0, "total": 0}
    ordered = sorted(values)
    return {
        "mean": round(statistics.mean(values), 2),
        "median": round(statistics.median(values), 2),
        "p95": ordered[max(0, (95 * len(ordered) + 99) // 100 - 1)],
        "total": sum(values),
    }


def historical_token_cost(
    anchors: Sequence[ToolCall], stores: dict[str, ReadOnlyStores]
) -> dict[str, Any]:
    candidates: dict[str, list[ToolCall]] = defaultdict(list)
    for anchor in anchors:
        if anchor.tool != "aft_search":
            continue
        top_k = anchor.input.get("topK", 10)
        if top_k != 10:
            continue
        rows = [row for row in parse_rows(anchor.tool, anchor.output) if row.symbol]
        if rows:
            candidates[anchor.repository].append(anchor)
    rng = random.Random(SEED + 1)
    labels = sorted(candidates)
    quotas = {label: TOKEN_SEARCHES // len(labels) for label in labels}
    for label in labels[: TOKEN_SEARCHES % len(labels)]:
        quotas[label] += 1
    selected = []
    for label in labels:
        selected.extend(rng.sample(candidates[label], min(quotas[label], len(candidates[label]))))

    per_search: dict[str, list[int]] = defaultdict(list)
    coverage = Counter()
    rows_total = 0
    baseline_caller_suffix_rows = 0
    baseline_caller_suffix_searches = 0
    per_repo = Counter()
    for anchor in selected:
        store = stores.get(anchor.repository)
        byte_counts = Counter()
        rows = [row for row in parse_rows(anchor.tool, anchor.output) if row.symbol][:10]
        suffix_count = len(re.findall(r"↩\d+", anchor.output))
        baseline_caller_suffix_rows += suffix_count
        baseline_caller_suffix_searches += int(suffix_count > 0)
        rows_total += len(rows)
        per_repo[repo_label(anchor.repository)] += 1
        for row in rows:
            facts = store.facts_for(row) if store is not None else RowFacts()
            suffixes = facts.suffixes()
            for fact, suffix in suffixes.items():
                byte_counts[fact] += len(suffix.encode())
                coverage[fact] += int(bool(suffix))
        for fact, count in byte_counts.items():
            per_search[fact].append(count)
    return {
        "searches": len(selected),
        "rows": rows_total,
        "top_k": 10,
        "baseline_caller_suffix_rows": baseline_caller_suffix_rows,
        "baseline_searches_with_caller_suffix": baseline_caller_suffix_searches,
        "by_repository": dict(sorted(per_repo.items())),
        "added_bytes_per_search": {fact: quantiles(values) for fact, values in sorted(per_search.items())},
        "rows_with_fact": dict(sorted(coverage.items())),
        "byte_to_token_estimate": "bytes / 4 (reported only as an estimate; no tokenizer was assumed)",
    }


def store_shape_cost(storage: Path) -> dict[str, Any]:
    output = {}
    rng = random.Random(SEED + 2)
    for label, root in STORE_SHAPE_REPOSITORIES.items():
        store = ReadOnlyStores(root, storage)
        try:
            rows = []
            for path, nodes in store.nodes_by_file.items():
                for node in nodes:
                    rows.append(ResultRow(path, node["scoped_name"], node["start_line"], node["end_line"]))
            sample = rng.sample(rows, min(1_667, len(rows)))
            byte_counts: dict[str, list[int]] = defaultdict(list)
            coverage = Counter()
            for offset in range(0, len(sample), 10):
                search_rows = sample[offset : offset + 10]
                totals = Counter()
                for row in search_rows:
                    suffixes = store.facts_for(row).suffixes()
                    for fact, suffix in suffixes.items():
                        totals[fact] += len(suffix.encode())
                        coverage[fact] += int(bool(suffix))
                for fact, value in totals.items():
                    byte_counts[fact].append(value)
            output[label] = {
                "root": str(root),
                "synthetic_top10_rows": len(sample),
                "row_groups": (len(sample) + 9) // 10,
                "added_bytes_per_group": {
                    fact: quantiles(values) for fact, values in sorted(byte_counts.items())
                },
                "rows_with_fact": dict(sorted(coverage.items())),
                "note": "shape check over real callgraph nodes, not agent-issued search results",
            }
        finally:
            store.close()
    return output


def repo_label(path: str) -> str:
    target = Path(path)
    for label, root in {**REPOSITORIES, **STORE_SHAPE_REPOSITORIES}.items():
        if target == root:
            return label
    return target.name or path


def write_sample_csv(path: Path, rows: Sequence[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=list(rows[0]) if rows else ["anchor_id"])
        writer.writeheader()
        writer.writerows(rows)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=Path, default=DEFAULT_DB)
    parser.add_argument("--aft-storage", type=Path, default=DEFAULT_AFT_STORAGE)
    parser.add_argument("--end-ms", type=int, help="inclusive snapshot cutoff; defaults to newest completed search")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--sample-csv", type=Path, required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    db = open_ro_sqlite(args.db)
    try:
        end_ms = args.end_ms
        if end_ms is None:
            end_ms = int(db.execute(
                """SELECT MAX(time_created) FROM part
                   WHERE json_extract(data,'$.tool')='aft_search'
                     AND json_extract(data,'$.state.status')='completed'"""
            ).fetchone()[0])
        start_ms = end_ms - 30 * DAY_MS
        calls = load_calls(db, start_ms, end_ms)
        selected_paths = {str(path) for path in REPOSITORIES.values()}
        anchors, events = build_events(calls, selected_paths)
        stores = {path: ReadOnlyStores(Path(path), args.aft_storage) for path in selected_paths}
        try:
            sample_rows, sample_summary = build_sample(events, stores)
            result = {
                "snapshot": {
                    "start_ms": start_ms,
                    "end_ms": end_ms,
                    "start_utc": datetime.fromtimestamp(start_ms / 1000, timezone.utc).isoformat(),
                    "end_utc": datetime.fromtimestamp(end_ms / 1000, timezone.utc).isoformat(),
                    "terminal_tool_calls": len(calls),
                },
                "repository_volume": [
                    dict(row)
                    for row in db.execute(
                        VOLUME_SQL, {"start_ms": start_ms, "end_ms": end_ms}
                    )
                ],
                "census": summarize_census(anchors, events),
                "sample": sample_summary,
                "historical_token_cost": historical_token_cost(anchors, stores),
                "store_shape_cross_language": store_shape_cost(args.aft_storage),
            }
        finally:
            for store in stores.values():
                store.close()
    finally:
        db.close()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    write_sample_csv(args.sample_csv, sample_rows)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
