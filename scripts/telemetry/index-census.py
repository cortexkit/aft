#!/usr/bin/env python3
"""Stream standing-index logs into deterministic per-root census artifacts.

Usage:
  index-census.py LOGS_DIR CACHE_KEYS_JSON [ROOT_FILTER]

The script reads inputs only. It writes `census-roots.csv` and
`census-summary.md` to `$CENSUS_OUTPUT_DIR` when set, otherwise to the current
working directory. `ROOT_FILTER` selects roots with that path prefix.
"""

from __future__ import annotations

import argparse
import csv
import glob
import json
import math
import os
import re
import subprocess
import sys
from collections import Counter, defaultdict, deque
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import DefaultDict, Iterable


# Exact sample: 2026-09-02T10:00:33Z [aft] [ses_f9e6fd934ffeTrgWkNC1ZDFQcg] callgraph cold-build decision: reason=corpus drift; action=force rebuild
DECISION_RE = re.compile(r"callgraph cold-build decision: reason=(?P<reason>.+?); action=(?P<action>.+)$")
# Exact sample: 2026-09-02T15:39:46Z [aft] resuming callgraph cold build from staged generation /Users/ufukaltinok/.local/share/cortexkit/aft/callgraph/bcf9718d69e7b23f/bcf9718d69e7b23f.staging.sqlite.tmp.resume
RESUME_RE = re.compile(r"resuming callgraph cold build from staged generation .*/callgraph/(?P<key>[0-9a-f]+)/")
# Exact sample: 2026-09-02T15:34:01Z [aft] callgraph cold build superseded, stopping after 40000/78811 (resolution)
SUPERSEDED_RE = re.compile(r"callgraph cold build superseded, stopping after (?P<done>\d+)/(?P<total>\d+) \((?P<stage>[^)]+)\)")
# No match in the input corpus sampled on 2026-09-02; retained so absence is counted rather than hidden.
COLD_START_RE = re.compile(r"callgraph cold build (?:started|start|begin|beginning)(?:\b|:)", re.IGNORECASE)
# No match in the input corpus sampled on 2026-09-02; retained so absence is counted rather than hidden.
COLD_PUBLISH_RE = re.compile(r"callgraph cold build .*?(?:published|publish|ready|completed|finished)(?:\b|:)", re.IGNORECASE)
# Exact sample: 2026-09-02T18:14:56Z [aft] [ses_019de471-4fdc-762d-9286-624dfad0b5fe] perf callgraph_store bounded cold_build: files=186 nodes=2897 refs=40671 edges=21460 committed_extracted_bytes=3523048 ms=230300
COLD_REPORTED_DURATION_RE = re.compile(
    r"perf callgraph_store bounded cold_build: files=(?P<files>\d+) nodes=(?P<nodes>\d+) refs=(?P<refs>\d+) edges=(?P<edges>\d+) committed_extracted_bytes=(?P<bytes>\d+) ms=(?P<ms>\d+)"
)
# Exact sample: 2026-09-02T04:41:30Z [aft] perf tier2_callgraph_snapshot: source=callgraph_store files=1357 exports=3081 edges=123428 entry_points=57 ms=9335
SNAPSHOT_RE = re.compile(r"perf tier2_callgraph_snapshot: source=(?P<source>\S+) files=(?P<files>\d+) exports=(?P<exports>\d+) edges=(?P<edges>\d+) entry_points=(?P<entry_points>\d+) ms=(?P<ms>\d+)")
# Exact sample: 2026-09-02T04:41:33Z [aft] perf tier2 category=dead_code reuse=miss ms=12031
TIER2_CATEGORY_RE = re.compile(r"perf tier2 category=(?P<category>\S+) reuse=(?P<reuse>hit|miss) ms=(?P<ms>\d+)")
# Exact sample: 2026-09-02T04:41:33Z [aft] perf tier2 phases category=dead_code freshness=3ms snapshot=9341ms scan=1326ms(923 files) db=41ms(lock=0,txn=41) rollup=1259ms
TIER2_PHASES_RE = re.compile(r"perf tier2 phases category=dead_code freshness=(?P<freshness>\d+)ms snapshot=(?P<snapshot>\d+)ms scan=(?P<scan>\d+)ms\((?P<files>\d+) files\) db=(?P<db>\d+)ms\(lock=(?P<lock>\d+),txn=(?P<txn>\d+)\) rollup=(?P<rollup>\d+)ms")
# Exact sample: 2026-09-02T09:01:28Z [aft] [ses___default__] semantic collect: 973 chunks from 39 files in 66 ms
SEMANTIC_COLLECT_RE = re.compile(r"semantic collect: (?P<chunks>\d+) chunks from (?P<files>\d+) files in (?P<ms>\d+) ms")
# Exact sample: 2026-09-02T09:01:28Z [aft] [ses___default__] semantic collect phases: sched=98ms read_hash=4ms parse=114ms extract=108ms build=3ms
SEMANTIC_PHASES_RE = re.compile(r"semantic collect phases: sched=(?P<sched>\d+)ms read_hash=(?P<read_hash>\d+)ms parse=(?P<parse>\d+)ms extract=(?P<extract>\d+)ms build=(?P<build>\d+)ms")
# Exact sample: 2026-09-02T04:08:49Z [aft] [ses___default__] semantic index build: embedding backend unavailable (openai compatible request failed: error sending request for url (http://localhost:1234/v1/embeddings): client error (Connect): tcp connect error: Connection refused (os error 61)); retrying in 15s
SEMANTIC_EMBED_RE = re.compile(r"semantic index build: embedding backend unavailable .*?; retrying in (?P<seconds>\d+)s")
# Exact sample: 2026-09-02T04:23:54Z [aft] [ses___default__] search index cold streaming build: 772 files, 99711 trigrams, 267 ms (pool=8)
SEARCH_BUILD_RE = re.compile(r"search index cold streaming build: (?P<files>\d+) files, (?P<trigrams>\d+) trigrams, (?P<ms>\d+) ms \(pool=(?P<pool>\d+)\)")
# Exact sample: 2026-09-02T04:36:31Z [aft] [ses_13ae8f525ffeCnx9aTNmVDRBdR] slow tool_call name=bash_drain_completions channel=31 corr=14 total=5670ms queue=0 translate=0 exec=5670 format=0 finalize=0 egress=0 egress_enqueue=0 egress_queue=0 egress_prepare=0 egress_write=0 frame_bytes=285 writer_queue_depth=1 writer_active=false writer_queue_full=false reserve_timeouts=0 root=/Users/ufukaltinok/Work/OSS/pi-mono
SLOW_TOOL_RE = re.compile(r"slow tool_call name=(?P<tool>\S+) channel=\d+ corr=\d+ total=(?P<total>\d+)ms queue=(?P<queue>\d+) .*?\broot=(?P<root>\S+)")
SLOW_WAITING_ON_RE = re.compile(r"\bwaiting_on=(?P<waiting_on>\S+)")
# Causal waiting is a gate metric only for calls long enough to affect users.
SLOW_WAITING_THRESHOLD_MS = 2_000
INDEX_EVENT_RE = re.compile(r"\bindex_event(?P<body>(?: [a-z_]+=[^ =]+)+)")
# Exact sample: 2026-09-02T05:06:29Z [aft] inspect-triggered cold-build request queued behind concurrency cap (2): request=inspect:/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/8f93aad09f2535d0/bg_1d190f2f615098f5:1 kind=explicit inspect Tier-2 run
LIMITER_QUEUED_RE = re.compile(r"inspect-triggered cold-build request queued behind concurrency cap \(2\): request=inspect:(?P<root>.+):(?P<request>\d+) kind=explicit inspect Tier-2 run")
# Exact sample: 2026-09-02T05:06:30Z [aft] inspect-triggered cold-build slot acquired after 832ms wait: request=inspect:/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/8f93aad09f2535d0/bg_1d190f2f615098f5:5 kind=explicit inspect Tier-2 run
LIMITER_ACQUIRED_RE = re.compile(r"inspect-triggered cold-build slot acquired after (?P<ms>\d+)ms wait: request=inspect:(?P<root>.+):(?P<request>\d+) kind=explicit inspect Tier-2 run")
# Exact sample: 2026-09-02T16:21:24Z [aft] [ses_110d87916ffeDbfbAjhUgyL8Ps] tier2 refresh deferred by cold build limit: categories=["dead_code", "unused_exports", "duplicates", "cycles", "complexity"]
DEFERRED_RE = re.compile(r"tier2 refresh deferred by cold build limit: categories=(?P<categories>\[.*\])")
# No match in the input corpus sampled on 2026-09-02; retained so breaker/suspension absence is counted rather than hidden.
BREAKER_RE = re.compile(r"BuildDeathBreaker|\bSuspended\b|\bsuspension\b")
# Exact sample: 2026-09-02T04:08:47Z [aft] subc bg subscription: installed root=/Users/ufukaltinok/Work/Projects/CortexKit/anthropic-auth session=__default__ channel=2@1 cause=subscribe suppressed=0
ROOT_SESSION_RE = re.compile(r"\broot=(?P<root>\S+)\s+session=(?P<session>\S+)")
# Exact sample: 2026-09-02T04:08:47Z [aft] [ses___default__] project root set: /Users/ufukaltinok/Work/Projects/CortexKit/anthropic-auth
PROJECT_ROOT_RE = re.compile(r"project root set: (?P<root>\S+)")
SESSION_RE = re.compile(r"\[(?P<session>ses_[^\]]+)\]")
TIMESTAMP_RE = re.compile(r"^(?P<timestamp>\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z)")

PATTERN_NAMES = (
    "cold_build_decision",
    "cold_build_resume",
    "cold_build_superseded",
    "cold_build_start",
    "cold_build_publish_or_ready",
    "cold_build_reported_duration",
    "tier2_callgraph_snapshot",
    "tier2_category",
    "tier2_dead_code_phases",
    "semantic_collect_duration",
    "semantic_collect_phases",
    "semantic_embed_retry",
    "search_index_cold_build",
    "slow_tool_call",
    "limiter_queued",
    "limiter_slot_acquired",
    "tier2_refresh_deferred",
    "breaker_or_suspension",
    "index_event",
)

LANGUAGE_NAMES = {
    "c": "C",
    "cc": "C++",
    "cpp": "C++",
    "cs": "C#",
    "css": "CSS",
    "go": "Go",
    "h": "C/C++ header",
    "hpp": "C++ header",
    "html": "HTML",
    "java": "Java",
    "js": "JavaScript",
    "jsx": "JavaScript",
    "json": "JSON",
    "kt": "Kotlin",
    "lua": "Lua",
    "m": "Objective-C",
    "md": "Markdown",
    "mjs": "JavaScript",
    "php": "PHP",
    "py": "Python",
    "r": "R",
    "rb": "Ruby",
    "rs": "Rust",
    "scala": "Scala",
    "sh": "Shell",
    "sol": "Solidity",
    "sql": "SQL",
    "swift": "Swift",
    "toml": "TOML",
    "ts": "TypeScript",
    "tsx": "TSX",
    "vue": "Vue",
    "xml": "XML",
    "yaml": "YAML",
    "yml": "YAML",
}


@dataclass
class RootStats:
    decisions: Counter[str] = field(default_factory=Counter)
    resumes: int = 0
    supersessions: list[tuple[int, int, str]] = field(default_factory=list)
    cold_starts: list[datetime] = field(default_factory=list)
    cold_publishes: list[datetime] = field(default_factory=list)
    cold_pair_ms: list[int] = field(default_factory=list)
    cold_reported_ms: list[int] = field(default_factory=list)
    snapshots_ms: list[int] = field(default_factory=list)
    tier2_category: DefaultDict[str, Counter[str]] = field(default_factory=lambda: defaultdict(Counter))
    tier2_phases: list[dict[str, int]] = field(default_factory=list)
    semantic_collect_ms: list[int] = field(default_factory=list)
    semantic_phases: list[dict[str, int]] = field(default_factory=list)
    semantic_retries: int = 0
    search_build_ms: list[int] = field(default_factory=list)
    slow_by_tool: DefaultDict[str, list[int]] = field(default_factory=lambda: defaultdict(list))
    slow_over_10s_by_tool: Counter[str] = field(default_factory=Counter)
    limiter_queued: int = 0
    limiter_wait_ms: list[int] = field(default_factory=list)
    deferred: int = 0
    breaker_hits: int = 0
    index_start_to_ready_ms: DefaultDict[str, list[int]] = field(default_factory=lambda: defaultdict(list))
    index_ready_to_first_query_ms: DefaultDict[str, list[int]] = field(default_factory=lambda: defaultdict(list))
    index_superseded: Counter[str] = field(default_factory=Counter)
    index_failed: Counter[str] = field(default_factory=Counter)
    index_suspended: Counter[str] = field(default_factory=Counter)
    waiting_on: Counter[str] = field(default_factory=Counter)
    index_progress: DefaultDict[str, list[tuple[str, int]]] = field(default_factory=lambda: defaultdict(list))
    index_resolution_share_pct: list[float] = field(default_factory=list)


def apply_index_event(stats: RootStats, fields: dict[str, str]) -> None:
    plane = fields["plane"]
    kind = fields["kind"]
    build_id = fields.get("build_id", "")
    if kind == "build_progress" and build_id and "elapsed_ms" in fields:
        stats.index_progress[build_id].append((fields.get("stage", "unknown"), int(fields["elapsed_ms"])))
    elif kind == "build_ready" and "elapsed_ms" in fields:
        elapsed_ms = int(fields["elapsed_ms"])
        stats.index_start_to_ready_ms[plane].append(elapsed_ms)
        if plane == "callgraph" and build_id:
            progress = stats.index_progress.pop(build_id, [])
            resolution = [(index, elapsed) for index, (stage, elapsed) in enumerate(progress) if stage == "resolution"]
            if resolution and elapsed_ms > 0:
                first_index, first_elapsed = resolution[0]
                last_elapsed = resolution[-1][1]
                # Progress elapsed_ms is cumulative from build_started. Use the
                # preceding stage boundary when available so the first resolution
                # progress record includes the transition into that stage.
                start_elapsed = progress[first_index - 1][1] if first_index else first_elapsed
                duration_ms = max(0, last_elapsed - start_elapsed)
                stats.index_resolution_share_pct.append(100.0 * duration_ms / elapsed_ms)
    elif kind == "first_query" and "ready_to_first_query_ms" in fields:
        stats.index_ready_to_first_query_ms[plane].append(int(fields["ready_to_first_query_ms"]))
    elif kind == "build_superseded":
        stats.index_superseded[plane] += 1
    elif kind == "build_failed":
        stats.index_failed[plane] += 1
    elif kind == "build_suspended":
        stats.index_suspended[plane] += 1


@dataclass(frozen=True)
class RootInfo:
    root: str
    key: str
    git: bool
    kind: str
    repo: str
    exists: bool
    file_count: int | None
    top_languages: str
    workspace_shape: str


def normalized_session(raw: str) -> str:
    return raw if raw.startswith("ses_") else f"ses_{raw}"


def parse_index_event_fields(line: str) -> dict[str, str] | None:
    match = INDEX_EVENT_RE.search(line)
    if not match:
        return None
    fields: dict[str, str] = {}
    for token in match.group("body").split():
        key, _, value = token.partition("=")
        if key and value:
            fields[key] = value
    return fields if fields.get("kind") and fields.get("plane") else None


def parse_timestamp(line: str) -> datetime | None:
    match = TIMESTAMP_RE.match(line)
    if not match:
        return None
    return datetime.fromisoformat(match.group("timestamp").replace("Z", "+00:00"))


def percentile(values: Iterable[int], fraction: float) -> int | None:
    ordered = sorted(values)
    if not ordered:
        return None
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)]


def fmt_int(value: int | None) -> str:
    return "n/a" if value is None else str(value)


def fmt_ms_stats(values: Iterable[int]) -> str:
    values = list(values)
    if not values:
        return "n=0"
    return f"n={len(values)},p50={percentile(values, 0.50)}ms,p95={percentile(values, 0.95)}ms,max={max(values)}ms"


def fmt_compact_ms(values: Iterable[int]) -> str:
    values = list(values)
    if not values:
        return "0/n/a/n/a"
    return f"{len(values)}/{percentile(values, 0.50)}/{max(values)}"


def fmt_compact_pct(values: Iterable[float]) -> str:
    values = sorted(values)
    if not values:
        return "0/n/a/n/a"
    middle = values[max(0, math.ceil(len(values) * 0.50) - 1)]
    return f"{len(values)}/{middle:.3f}/{max(values):.3f}"


def clean_root(value: str) -> str:
    return value.rstrip(".,;)")


def root_from_session_line(line: str, session_roots: dict[str, str]) -> str | None:
    direct = re.search(r"\broot=(\S+)", line)
    if direct:
        return clean_root(direct.group(1))
    session = SESSION_RE.search(line)
    if session:
        return session_roots.get(session.group("session"))
    return None


def read_paths_from_git(root: Path) -> list[str] | None:
    command = ["git", "-C", str(root), "ls-files", "-z"]
    try:
        completed = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, check=False, timeout=30)
    except (OSError, subprocess.TimeoutExpired):
        return None
    if completed.returncode:
        return None
    return [path for path in completed.stdout.decode("utf-8", "replace").split("\0") if path]


def read_paths_from_find(root: Path) -> list[str] | None:
    command = [
        "find", str(root), "-type", "d", "(", "-name", "target", "-o", "-name", "node_modules", "-o", "-name", ".git", ")", "-prune", "-o", "-type", "f", "-print0",
    ]
    try:
        completed = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, check=False, timeout=60)
    except (OSError, subprocess.TimeoutExpired):
        return None
    if completed.returncode:
        return None
    prefix = f"{root}{os.sep}"
    return [path.removeprefix(prefix) for path in completed.stdout.decode("utf-8", "replace").split("\0") if path]


def top_languages(paths: Iterable[str]) -> str:
    counts: Counter[str] = Counter()
    for path in paths:
        name = Path(path).name
        if "." not in name or name.startswith("."):
            continue
        extension = name.rsplit(".", 1)[1].lower()
        counts[extension] += 1
    if not counts:
        return "n/a"
    rows = sorted(counts.items(), key=lambda pair: (-pair[1], pair[0]))[:3]
    return "; ".join(f".{ext} {LANGUAGE_NAMES.get(ext, ext)}={count}" for ext, count in rows)


def cargo_workspace_member_count(root: Path) -> int | None:
    manifest = root / "Cargo.toml"
    if not manifest.is_file():
        return None
    try:
        text = manifest.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    workspace = re.search(r"(?ms)^\[workspace\]\s*(.*?)(?=^\[|\Z)", text)
    if not workspace:
        return None
    member_list = re.search(r"(?ms)^members\s*=\s*\[(.*?)\]", workspace.group(1))
    if not member_list:
        return 0
    members = re.findall(r'"([^"]+)"', member_list.group(1))
    expanded: set[Path] = set()
    for member in members:
        for candidate in glob.glob(str(root / member), recursive=True):
            path = Path(candidate)
            if path.is_dir() and (path / "Cargo.toml").is_file():
                expanded.add(path.resolve())
    return len(expanded)


def node_workspace_member_count(root: Path) -> int | None:
    manifest = root / "package.json"
    if not manifest.is_file():
        return None
    try:
        payload = json.loads(manifest.read_text(encoding="utf-8", errors="replace"))
    except (OSError, json.JSONDecodeError):
        return None
    workspaces = payload.get("workspaces")
    if isinstance(workspaces, dict):
        workspaces = workspaces.get("packages")
    if not isinstance(workspaces, list):
        return None
    expanded: set[Path] = set()
    for pattern in workspaces:
        if not isinstance(pattern, str):
            continue
        for candidate in glob.glob(str(root / pattern), recursive=True):
            path = Path(candidate)
            if path.is_dir() and (path / "package.json").is_file():
                expanded.add(path.resolve())
    return len(expanded)


def workspace_shape(root: Path) -> str:
    pieces = []
    cargo = cargo_workspace_member_count(root)
    node = node_workspace_member_count(root)
    if cargo is not None:
        pieces.append(f"cargo:{cargo}")
    if node is not None:
        pieces.append(f"node:{node}")
    return "; ".join(pieces) if pieces else "none"


def load_roots(cache_keys_path: Path) -> tuple[dict[str, dict[str, object]], dict[str, str]]:
    with cache_keys_path.open(encoding="utf-8") as handle:
        raw = json.load(handle)
    if not isinstance(raw, dict):
        raise ValueError("cache-keys JSON must be an object mapping root paths to records")
    roots: dict[str, dict[str, object]] = {}
    for root, value in raw.items():
        if not isinstance(root, str) or not isinstance(value, dict):
            continue
        key = value.get("key")
        if isinstance(key, str):
            roots[root] = value
    primary_by_key: DefaultDict[str, list[str]] = defaultdict(list)
    for root, value in roots.items():
        if "/worktrees/" not in root:
            primary_by_key[str(value["key"])].append(root)
    repo_by_root: dict[str, str] = {}
    for root, value in roots.items():
        if "/worktrees/" not in root:
            repo_by_root[root] = Path(root).name
            continue
        primaries = sorted(primary_by_key[str(value["key"])])
        repo_by_root[root] = Path(primaries[0]).name if primaries else Path(root).name
    return roots, repo_by_root


def collect_shape(root: str, record: dict[str, object], repo: str) -> RootInfo:
    path = Path(root)
    exists = path.is_dir()
    git = bool(record.get("git_root_commit"))
    kind = "worktree" if "/worktrees/" in root else "primary"
    if not exists:
        return RootInfo(root, str(record["key"]), git, kind, repo, False, None, "n/a", "n/a")
    paths = read_paths_from_git(path) if git else read_paths_from_find(path)
    if paths is None:
        return RootInfo(root, str(record["key"]), git, kind, repo, True, None, "n/a", workspace_shape(path))
    return RootInfo(root, str(record["key"]), git, kind, repo, True, len(paths), top_languages(paths), workspace_shape(path))


def build_session_roots(log_files: list[Path]) -> dict[str, str]:
    observed: DefaultDict[str, Counter[str]] = defaultdict(Counter)
    for path in log_files:
        with path.open(encoding="utf-8", errors="replace") as handle:
            for line in handle:
                match = ROOT_SESSION_RE.search(line)
                if match:
                    observed[normalized_session(match.group("session"))][clean_root(match.group("root"))] += 1
                project = PROJECT_ROOT_RE.search(line)
                session = SESSION_RE.search(line)
                if project and session:
                    observed[session.group("session")][clean_root(project.group("root"))] += 1
    resolved: dict[str, str] = {}
    for session, roots in observed.items():
        if len(roots) == 1:
            resolved[session] = next(iter(roots))
    return resolved


def pair_cold_events(stats: RootStats) -> None:
    starts = deque(sorted(stats.cold_starts))
    for published in sorted(stats.cold_publishes):
        while starts and starts[0] > published:
            starts.popleft()
        if starts:
            stats.cold_pair_ms.append(int((published - starts.popleft()).total_seconds() * 1000))


def format_reasons(reasons: Counter[str]) -> str:
    return "; ".join(f"{reason}={count}" for reason, count in sorted(reasons.items())) or "none"


def format_supersessions(events: list[tuple[int, int, str]]) -> str:
    if not events:
        return "0"
    return "; ".join(f"{done}/{total}={done / total:.1%}@{stage}" if total else f"{done}/0@n/a@{stage}" for done, total, stage in events)


def format_slow(stats: RootStats) -> str:
    if not stats.slow_by_tool:
        return "none"
    return "; ".join(
        f"{tool}:n={len(values)},p50={percentile(values, 0.50)}ms,p95={percentile(values, 0.95)}ms"
        for tool, values in sorted(stats.slow_by_tool.items())
    )


def size_bucket(files: int | None) -> str:
    if files is None:
        return "unknown"
    if files < 2_000:
        return "<2k"
    if files < 10_000:
        return "2k-10k"
    if files < 50_000:
        return "10k-50k"
    return ">50k"


def write_csv(path: Path, infos: list[RootInfo], stats: dict[str, RootStats]) -> None:
    headers = [
        "root", "repo", "kind", "git", "exists", "files", "top_languages", "workspace_shape",
        "cold_builds_reported", "cold_build_pairs", "cold_wall_p50_ms", "cold_wall_max_ms", "resumes",
        "supersessions", "decision_reasons", "tier2_snapshot_ms_n_p50_max", "tier2_category_reuse",
        "semantic_collect_ms_n_p50_max", "semantic_embed_retries", "search_build_ms_n_p50_max",
        "slow_calls_p50_p95_by_tool", "slow_calls_over_10s_by_tool", "limiter_queued", "limiter_wait_ms_n_p95_max",
        "tier2_deferred", "breaker_or_suspension_hits",
        "index_search_start_to_ready_ms_n_p50_max", "index_search_ready_to_first_query_ms_n_p50_max",
        "index_callgraph_start_to_ready_ms_n_p50_max", "index_callgraph_ready_to_first_query_ms_n_p50_max",
        "index_callgraph_resolution_share_pct_n_p50_max",
        "index_semantic_start_to_ready_ms_n_p50_max", "index_semantic_ready_to_first_query_ms_n_p50_max",
        "index_search_superseded", "index_search_failed", "index_search_suspended",
        "index_callgraph_superseded", "index_callgraph_failed", "index_callgraph_suspended",
        "index_semantic_superseded", "index_semantic_failed", "index_semantic_suspended",
        "index_waiting_on",
    ]
    with path.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=headers, lineterminator="\n")
        writer.writeheader()
        for info in infos:
            root_stats = stats[info.root]
            cold_wall = root_stats.cold_pair_ms or root_stats.cold_reported_ms
            tier2_reuse = "; ".join(
                f"{category}:hit={values['hit']},miss={values['miss']}"
                for category, values in sorted(root_stats.tier2_category.items())
            ) or "none"
            slow_over = "; ".join(f"{tool}={count}" for tool, count in sorted(root_stats.slow_over_10s_by_tool.items())) or "none"
            writer.writerow({
                "root": info.root,
                "repo": info.repo,
                "kind": info.kind,
                "git": "yes" if info.git else "no",
                "exists": "yes" if info.exists else "no",
                "files": "n/a" if info.file_count is None else info.file_count,
                "top_languages": info.top_languages,
                "workspace_shape": info.workspace_shape,
                "cold_builds_reported": len(root_stats.cold_reported_ms),
                "cold_build_pairs": len(root_stats.cold_pair_ms),
                "cold_wall_p50_ms": fmt_int(percentile(cold_wall, 0.50)),
                "cold_wall_max_ms": fmt_int(max(cold_wall) if cold_wall else None),
                "resumes": root_stats.resumes,
                "supersessions": format_supersessions(root_stats.supersessions),
                "decision_reasons": format_reasons(root_stats.decisions),
                "tier2_snapshot_ms_n_p50_max": fmt_compact_ms(root_stats.snapshots_ms),
                "tier2_category_reuse": tier2_reuse,
                "semantic_collect_ms_n_p50_max": fmt_compact_ms(root_stats.semantic_collect_ms),
                "semantic_embed_retries": root_stats.semantic_retries,
                "search_build_ms_n_p50_max": fmt_compact_ms(root_stats.search_build_ms),
                "slow_calls_p50_p95_by_tool": format_slow(root_stats),
                "slow_calls_over_10s_by_tool": slow_over,
                "limiter_queued": root_stats.limiter_queued,
                "limiter_wait_ms_n_p95_max": fmt_compact_ms(root_stats.limiter_wait_ms) if not root_stats.limiter_wait_ms else f"{len(root_stats.limiter_wait_ms)}/{percentile(root_stats.limiter_wait_ms, 0.95)}/{max(root_stats.limiter_wait_ms)}",
                "tier2_deferred": root_stats.deferred,
                "breaker_or_suspension_hits": root_stats.breaker_hits,
                "index_search_start_to_ready_ms_n_p50_max": fmt_compact_ms(root_stats.index_start_to_ready_ms["search"]),
                "index_search_ready_to_first_query_ms_n_p50_max": fmt_compact_ms(root_stats.index_ready_to_first_query_ms["search"]),
                "index_callgraph_start_to_ready_ms_n_p50_max": fmt_compact_ms(root_stats.index_start_to_ready_ms["callgraph"]),
                "index_callgraph_ready_to_first_query_ms_n_p50_max": fmt_compact_ms(root_stats.index_ready_to_first_query_ms["callgraph"]),
                "index_callgraph_resolution_share_pct_n_p50_max": fmt_compact_pct(root_stats.index_resolution_share_pct),
                "index_semantic_start_to_ready_ms_n_p50_max": fmt_compact_ms(root_stats.index_start_to_ready_ms["semantic"]),
                "index_semantic_ready_to_first_query_ms_n_p50_max": fmt_compact_ms(root_stats.index_ready_to_first_query_ms["semantic"]),
                "index_search_superseded": root_stats.index_superseded["search"],
                "index_search_failed": root_stats.index_failed["search"],
                "index_search_suspended": root_stats.index_suspended["search"],
                "index_callgraph_superseded": root_stats.index_superseded["callgraph"],
                "index_callgraph_failed": root_stats.index_failed["callgraph"],
                "index_callgraph_suspended": root_stats.index_suspended["callgraph"],
                "index_semantic_superseded": root_stats.index_superseded["semantic"],
                "index_semantic_failed": root_stats.index_failed["semantic"],
                "index_semantic_suspended": root_stats.index_suspended["semantic"],
                "index_waiting_on": format_reasons(root_stats.waiting_on),
            })


def write_summary(
    path: Path,
    infos: list[RootInfo],
    stats: dict[str, RootStats],
    pattern_counts: Counter[str],
    unassigned: Counter[str],
    log_files: list[Path],
    first_timestamp: datetime | None,
    last_timestamp: datetime | None,
) -> None:
    observed_infos = [info for info in infos if any((
        stats[info.root].decisions, stats[info.root].resumes, stats[info.root].supersessions,
        stats[info.root].cold_reported_ms, stats[info.root].snapshots_ms, stats[info.root].slow_by_tool,
        stats[info.root].limiter_queued, stats[info.root].limiter_wait_ms, stats[info.root].deferred,
    ))]
    all_slow_over_10s = Counter()
    limiter_waits: list[int] = []
    for root_stats in stats.values():
        all_slow_over_10s.update(root_stats.slow_over_10s_by_tool)
        limiter_waits.extend(root_stats.limiter_wait_ms)
    with path.open("w", encoding="utf-8") as handle:
        handle.write("# Standing-index log census\n\n")
        handle.write("## Inputs and method\n\n")
        handle.write(f"- Log files: {len(log_files)} (`aft-*.log`), streamed line by line.\n")
        handle.write(f"- Cache-key roots: {len(infos)}. Selected roots: {len(infos)}.\n")
        handle.write(f"- Log timestamps: {first_timestamp.isoformat() if first_timestamp else 'n/a'} to {last_timestamp.isoformat() if last_timestamp else 'n/a'}.\n")
        handle.write("- `cold_wall_*` uses paired literal start/publish events when present; if none pair, it uses the daemon's direct `perf ... cold_build ... ms=N` duration and the CSV exposes both counts.\n")
        handle.write("- Session attribution accepts only sessions observed with exactly one root. Events without an in-line root, an unambiguous session binding, or a uniquely resolvable cache key remain unassigned.\n")
        handle.write("- Percentiles use nearest-rank (`ceil(p*n)`) over the recorded log samples.\n\n")
        handle.write("## Pattern coverage\n\n| Pattern family | Matches |\n| --- | ---: |\n")
        for name in PATTERN_NAMES:
            handle.write(f"| `{name}` | {pattern_counts[name]} |\n")
        handle.write("\n## Per-root table\n\n")
        handle.write("The complete one-row-per-root table is `census-roots.csv`; this compact table retains every root with a standing-index or recorded slow-call event. `cold` is `reported/pairs; p50/max ms`; snapshot is `n/p50/max ms`; limiter is `n/p95/max ms`.\n\n")
        handle.write("| Repo | Kind | Git? | Files | Languages | Workspace | Cold | Resumes | Supersessions | Decisions | Tier2 snapshot | Slow calls p50/p95 by tool | Limiter |\n")
        handle.write("| --- | --- | --- | ---: | --- | --- | --- | ---: | --- | --- | --- | --- | --- |\n")
        for info in observed_infos:
            root_stats = stats[info.root]
            cold_wall = root_stats.cold_pair_ms or root_stats.cold_reported_ms
            cold = f"{len(root_stats.cold_reported_ms)}/{len(root_stats.cold_pair_ms)}; {fmt_int(percentile(cold_wall, 0.50))}/{fmt_int(max(cold_wall) if cold_wall else None)}"
            files = "n/a" if info.file_count is None else str(info.file_count)
            limiter = "0/n/a/n/a" if not root_stats.limiter_wait_ms else f"{len(root_stats.limiter_wait_ms)}/{percentile(root_stats.limiter_wait_ms, 0.95)}/{max(root_stats.limiter_wait_ms)}"
            handle.write(
                f"| {info.repo} (`{Path(info.root).name}`) | {info.kind} | {'yes' if info.git else 'no'} | {files} | {info.top_languages} | {info.workspace_shape} | {cold} | {root_stats.resumes} | {format_supersessions(root_stats.supersessions)} | {format_reasons(root_stats.decisions)} | {fmt_compact_ms(root_stats.snapshots_ms)} | {format_slow(root_stats)} | {limiter} |\n"
            )
        handle.write("\n## Per-shape rollup\n\n")
        handle.write("| Size bucket | Kind | Roots | Roots with cold marker | Reported cold builds | Cold wall p50/max ms | >10s recorded slow calls | Limiter wait p95/max ms |\n")
        handle.write("| --- | --- | ---: | ---: | ---: | --- | ---: | --- |\n")
        buckets = ("<2k", "2k-10k", "10k-50k", ">50k", "unknown")
        for bucket in buckets:
            for kind in ("primary", "worktree"):
                group = [info for info in infos if size_bucket(info.file_count) == bucket and info.kind == kind]
                cold_marked = [info for info in group if stats[info.root].decisions or stats[info.root].resumes or stats[info.root].supersessions or stats[info.root].cold_reported_ms]
                cold = [duration for info in group for duration in stats[info.root].cold_reported_ms]
                slow = [duration for info in group for values in stats[info.root].slow_by_tool.values() for duration in values if duration > 10_000]
                waits = [duration for info in group for duration in stats[info.root].limiter_wait_ms]
                handle.write(
                    f"| {bucket} | {kind} | {len(group)} | {len(cold_marked)} | {len(cold)} | {fmt_int(percentile(cold, 0.50))}/{fmt_int(max(cold) if cold else None)} | {len(slow)} | {fmt_int(percentile(waits, 0.95))}/{fmt_int(max(waits) if waits else None)} |\n"
                )
        handle.write("\n## Attribution gaps\n\n")
        handle.write("| Family | Unassigned matches | Why |\n| --- | ---: | --- |\n")
        for name in PATTERN_NAMES:
            if pattern_counts[name]:
                handle.write(f"| `{name}` | {unassigned[name]} | no in-line root, uniquely bound session, or uniquely resolvable key |\n")
        handle.write("\n## Recorded >10s tool calls\n\n")
        handle.write(f"{sum(all_slow_over_10s.values())} `slow tool_call` records exceeded 10 seconds: ")
        handle.write(", ".join(f"`{tool}`={count}" for tool, count in sorted(all_slow_over_10s.items())) or "none")
        handle.write(". The log shape records timing but no causal `build_state` field, so this is an observed wait count, not a claim that every wait was caused by cold building.\n")
        handle.write(
            f"Inspect limiter: {pattern_counts['limiter_queued']} queued, {len(limiter_waits)} acquired; "
            f"p95={fmt_int(percentile(limiter_waits, 0.95))}ms, max={fmt_int(max(limiter_waits) if limiter_waits else None)}ms.\n"
        )


def main() -> int:
    parser = argparse.ArgumentParser(description="Census standing-index telemetry without changing daemon state.")
    parser.add_argument("logs_dir", type=Path)
    parser.add_argument("cache_keys", type=Path)
    parser.add_argument("root_filter", nargs="?", help="Optional absolute root-path prefix to include")
    args = parser.parse_args()
    if not args.logs_dir.is_dir():
        parser.error(f"logs directory does not exist: {args.logs_dir}")
    if not args.cache_keys.is_file():
        parser.error(f"cache-keys file does not exist: {args.cache_keys}")

    cache_roots, repo_by_root = load_roots(args.cache_keys)
    selected_roots = sorted(root for root in cache_roots if not args.root_filter or root.startswith(args.root_filter))
    if not selected_roots:
        parser.error("root filter selected no cache-key roots")
    selected_set = set(selected_roots)
    infos = [collect_shape(root, cache_roots[root], repo_by_root[root]) for root in selected_roots]
    stats = {root: RootStats() for root in selected_roots}
    keys_to_roots: DefaultDict[str, list[str]] = defaultdict(list)
    primary_keys: DefaultDict[str, list[str]] = defaultdict(list)
    for root, record in cache_roots.items():
        keys_to_roots[str(record["key"])].append(root)
        if "/worktrees/" not in root:
            primary_keys[str(record["key"])].append(root)

    log_files = sorted(path for path in args.logs_dir.glob("aft-*.log") if path.is_file())
    session_roots = build_session_roots(log_files)
    pattern_counts: Counter[str] = Counter({name: 0 for name in PATTERN_NAMES})
    unassigned: Counter[str] = Counter({name: 0 for name in PATTERN_NAMES})
    first_timestamp: datetime | None = None
    last_timestamp: datetime | None = None

    def assign(root: str | None, family: str) -> RootStats | None:
        if root in selected_set:
            return stats[root]
        unassigned[family] += 1
        return None

    def root_for_key(key: str) -> str | None:
        primaries = sorted(primary_keys.get(key, []))
        if len(primaries) == 1:
            return primaries[0]
        roots = sorted(keys_to_roots.get(key, []))
        return roots[0] if len(roots) == 1 else None

    for path in log_files:
        with path.open(encoding="utf-8", errors="replace") as handle:
            for line in handle:
                timestamp = parse_timestamp(line)
                if timestamp:
                    first_timestamp = timestamp if first_timestamp is None or timestamp < first_timestamp else first_timestamp
                    last_timestamp = timestamp if last_timestamp is None or timestamp > last_timestamp else last_timestamp
                event_root = root_from_session_line(line, session_roots)

                match = DECISION_RE.search(line)
                if match:
                    pattern_counts["cold_build_decision"] += 1
                    target = assign(event_root, "cold_build_decision")
                    if target:
                        target.decisions[match.group("reason")] += 1
                match = RESUME_RE.search(line)
                if match:
                    pattern_counts["cold_build_resume"] += 1
                    target = assign(root_for_key(match.group("key")), "cold_build_resume")
                    if target:
                        target.resumes += 1
                match = SUPERSEDED_RE.search(line)
                if match:
                    pattern_counts["cold_build_superseded"] += 1
                    target = assign(event_root, "cold_build_superseded")
                    if target:
                        target.supersessions.append((int(match.group("done")), int(match.group("total")), match.group("stage")))
                if COLD_START_RE.search(line):
                    pattern_counts["cold_build_start"] += 1
                    target = assign(event_root, "cold_build_start")
                    if target and timestamp:
                        target.cold_starts.append(timestamp)
                if COLD_PUBLISH_RE.search(line):
                    pattern_counts["cold_build_publish_or_ready"] += 1
                    target = assign(event_root, "cold_build_publish_or_ready")
                    if target and timestamp:
                        target.cold_publishes.append(timestamp)
                match = COLD_REPORTED_DURATION_RE.search(line)
                if match:
                    pattern_counts["cold_build_reported_duration"] += 1
                    target = assign(event_root, "cold_build_reported_duration")
                    if target:
                        target.cold_reported_ms.append(int(match.group("ms")))
                match = SNAPSHOT_RE.search(line)
                if match:
                    pattern_counts["tier2_callgraph_snapshot"] += 1
                    target = assign(event_root, "tier2_callgraph_snapshot")
                    if target:
                        target.snapshots_ms.append(int(match.group("ms")))
                match = TIER2_CATEGORY_RE.search(line)
                if match:
                    pattern_counts["tier2_category"] += 1
                    target = assign(event_root, "tier2_category")
                    if target:
                        target.tier2_category[match.group("category")][match.group("reuse")] += 1
                match = TIER2_PHASES_RE.search(line)
                if match:
                    pattern_counts["tier2_dead_code_phases"] += 1
                    target = assign(event_root, "tier2_dead_code_phases")
                    if target:
                        target.tier2_phases.append({key: int(value) for key, value in match.groupdict().items()})
                match = SEMANTIC_COLLECT_RE.search(line)
                if match:
                    pattern_counts["semantic_collect_duration"] += 1
                    target = assign(event_root, "semantic_collect_duration")
                    if target:
                        target.semantic_collect_ms.append(int(match.group("ms")))
                match = SEMANTIC_PHASES_RE.search(line)
                if match:
                    pattern_counts["semantic_collect_phases"] += 1
                    target = assign(event_root, "semantic_collect_phases")
                    if target:
                        target.semantic_phases.append({key: int(value) for key, value in match.groupdict().items()})
                match = SEMANTIC_EMBED_RE.search(line)
                if match:
                    pattern_counts["semantic_embed_retry"] += 1
                    target = assign(event_root, "semantic_embed_retry")
                    if target:
                        target.semantic_retries += 1
                match = SEARCH_BUILD_RE.search(line)
                if match:
                    pattern_counts["search_index_cold_build"] += 1
                    target = assign(event_root, "search_index_cold_build")
                    if target:
                        target.search_build_ms.append(int(match.group("ms")))
                match = SLOW_TOOL_RE.search(line)
                if match:
                    pattern_counts["slow_tool_call"] += 1
                    target = assign(clean_root(match.group("root")), "slow_tool_call")
                    if target:
                        duration = int(match.group("total"))
                        tool = match.group("tool")
                        target.slow_by_tool[tool].append(duration)
                        if duration > 10_000:
                            target.slow_over_10s_by_tool[tool] += 1
                match = LIMITER_QUEUED_RE.search(line)
                if match:
                    pattern_counts["limiter_queued"] += 1
                    target = assign(clean_root(match.group("root")), "limiter_queued")
                    if target:
                        target.limiter_queued += 1
                match = LIMITER_ACQUIRED_RE.search(line)
                if match:
                    pattern_counts["limiter_slot_acquired"] += 1
                    target = assign(clean_root(match.group("root")), "limiter_slot_acquired")
                    if target:
                        target.limiter_wait_ms.append(int(match.group("ms")))
                match = DEFERRED_RE.search(line)
                if match:
                    pattern_counts["tier2_refresh_deferred"] += 1
                    target = assign(event_root, "tier2_refresh_deferred")
                    if target:
                        target.deferred += 1
                if BREAKER_RE.search(line):
                    pattern_counts["breaker_or_suspension"] += 1
                    target = assign(event_root, "breaker_or_suspension")
                    if target:
                        target.breaker_hits += 1
                index_fields = parse_index_event_fields(line)
                if index_fields:
                    pattern_counts["index_event"] += 1
                    target = assign(clean_root(index_fields.get("root", "")) or event_root, "index_event")
                    if target:
                        apply_index_event(target, index_fields)
                if match := SLOW_WAITING_ON_RE.search(line):
                    waiting_root = event_root
                    slow = SLOW_TOOL_RE.search(line)
                    if slow:
                        waiting_root = clean_root(slow.group("root"))
                    target = assign(waiting_root, "slow_tool_call")
                    if target and slow and int(slow.group("total")) > SLOW_WAITING_THRESHOLD_MS:
                        target.waiting_on[match.group("waiting_on")] += 1

    for root_stats in stats.values():
        pair_cold_events(root_stats)

    output_dir = Path(os.environ.get("CENSUS_OUTPUT_DIR", ".")).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    csv_path = output_dir / "census-roots.csv"
    summary_path = output_dir / "census-summary.md"
    write_csv(csv_path, infos, stats)
    write_summary(summary_path, infos, stats, pattern_counts, unassigned, log_files, first_timestamp, last_timestamp)

    print(f"wrote {csv_path}")
    print(f"wrote {summary_path}")
    print("pattern match counts:")
    for name in PATTERN_NAMES:
        print(f"{name}={pattern_counts[name]}")
    return 0


def self_test() -> None:
    lines = [
        "2026-09-03T00:00:00Z [aft] index_event kind=build_started plane=search build_id=b-1-1 root=/tmp/proj key=abc",
        "2026-09-03T00:00:01Z [aft] index_event kind=build_progress plane=search build_id=b-1-1 root=/tmp/proj key=abc stage=streaming completed=1 total=1 elapsed_ms=12",
        "2026-09-03T00:00:02Z [aft] index_event kind=build_ready plane=search build_id=b-1-1 root=/tmp/proj key=abc elapsed_ms=25 files=3 trigrams=9",
        "2026-09-03T00:00:03Z [aft] index_event kind=first_query plane=search build_id=b-1-1 root=/tmp/proj key=abc tool=grep queue_ms=1 service_ms=4 status=ok ready_to_first_query_ms=40",
        "2026-09-03T00:00:04Z [aft] index_event kind=build_started plane=callgraph build_id=b-1-5 root=/tmp/proj key=abc",
        "2026-09-03T00:00:05Z [aft] index_event kind=build_progress plane=callgraph build_id=b-1-5 root=/tmp/proj key=abc stage=symbol-export-index completed=1 total=1 elapsed_ms=10",
        "2026-09-03T00:00:06Z [aft] index_event kind=build_progress plane=callgraph build_id=b-1-5 root=/tmp/proj key=abc stage=resolution completed=0 total=2 elapsed_ms=20",
        "2026-09-03T00:00:07Z [aft] index_event kind=build_progress plane=callgraph build_id=b-1-5 root=/tmp/proj key=abc stage=resolution completed=2 total=2 elapsed_ms=50",
        "2026-09-03T00:00:08Z [aft] index_event kind=build_ready plane=callgraph build_id=b-1-5 root=/tmp/proj key=abc elapsed_ms=100",
        "2026-09-03T00:00:09Z [aft] index_event kind=build_started plane=callgraph build_id=b-1-2 root=/tmp/proj key=abc",
        "2026-09-03T00:00:12Z [aft] index_event kind=build_superseded plane=callgraph build_id=b-1-2 root=/tmp/proj key=abc stage=inventory",
        "2026-09-03T00:00:13Z [aft] index_event kind=build_failed plane=semantic build_id=b-1-3 root=/tmp/proj key=abc reason=denied",
        "2026-09-03T00:00:14Z [aft] index_event kind=build_suspended plane=callgraph build_id=b-1-4 root=/tmp/other key=def reason=breaker",
        "2026-09-03T00:00:10Z [aft] slow tool_call name=grep channel=1 corr=1 total=12000ms queue=0 translate=0 exec=12000 format=0 finalize=0 egress=0 egress_enqueue=0 egress_queue=0 egress_prepare=0 egress_write=0 frame_bytes=1 writer_queue_depth=1 writer_active=false writer_queue_full=false reserve_timeouts=0 waiting_on=build waiting_on_build_id=b-1-1 wait_ms=8000 root=/tmp/proj",
        "2026-09-03T00:00:11Z [aft] slow tool_call name=grep channel=1 corr=2 total=1500ms queue=0 translate=0 exec=1500 format=0 finalize=0 egress=0 egress_enqueue=0 egress_queue=0 egress_prepare=0 egress_write=0 frame_bytes=1 writer_queue_depth=1 writer_active=false writer_queue_full=false reserve_timeouts=0 waiting_on=build waiting_on_build_id=b-1-1 wait_ms=1000 root=/tmp/proj",
    ]
    by_root: dict[str, RootStats] = {}
    for line in lines:
        fields = parse_index_event_fields(line)
        if fields:
            stats = by_root.setdefault(fields["root"], RootStats())
            apply_index_event(stats, fields)
        waiting = SLOW_WAITING_ON_RE.search(line)
        slow = SLOW_TOOL_RE.search(line)
        if waiting and slow and int(slow.group("total")) > SLOW_WAITING_THRESHOLD_MS:
            stats = by_root.setdefault(clean_root(slow.group("root")), RootStats())
            stats.waiting_on[waiting.group("waiting_on")] += 1
    proj = by_root["/tmp/proj"]
    assert proj.index_start_to_ready_ms["search"] == [25], proj.index_start_to_ready_ms
    assert proj.index_ready_to_first_query_ms["search"] == [40], proj.index_ready_to_first_query_ms
    assert proj.index_superseded["callgraph"] == 1, proj.index_superseded
    assert proj.index_failed["semantic"] == 1, proj.index_failed
    assert proj.index_resolution_share_pct == [40.0], proj.index_resolution_share_pct
    assert proj.waiting_on["build"] == 1, proj.waiting_on
    other = by_root["/tmp/other"]
    assert other.index_suspended["callgraph"] == 1, other.index_suspended
    print("index-census self-test passed")


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--self-test":
        self_test()
        raise SystemExit(0)
    raise SystemExit(main())
