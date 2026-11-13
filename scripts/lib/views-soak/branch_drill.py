#!/usr/bin/env python3
# Reads pre-0.64 view storage; does not run against AFT 0.64 or later.
"""Exercise branch switches in an isolated source-checkout session."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import asdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable, Mapping

from common import (
    NdjsonClient,
    ProcessSample,
    SoakError,
    SwitchProbe,
    ToolClient,
    assert_clean_worktree,
    current_generation,
    extract_embedding_calls,
    find_subc_daemon_pid,
    git_bytes,
    git_text,
    health_snapshot,
    index_event_root_matches,
    manifest_entry_count,
    manifest_fingerprint,
    markdown_cell,
    resolve_root,
    run_checked,
    sample_process,
    select_stable_symbols,
    utc_now,
    wait_cold_work_ready,
    wait_indexes_ready,
    write_json,
)


REPO_ROOT = Path(__file__).resolve().parents[3]
RESULT_DIR = REPO_ROOT / "docs" / "investigations" / "views-soak-2026-09"
OPENCODE_ROOT = Path.home() / "Work" / "OSS" / "opencode"
REUSE_RE = re.compile(
    r"content-addressed view HEAD reuse (?P<reused>\d+)/(?P<total>\d+) root=(?P<root>.+)$"
)
PUBLICATION_RE = re.compile(
    r"content-addressed view publication(?: after semantic refresh)? published=(?P<published>true|false) "
    r"blob_puts=(?P<puts>\d+) pending_paths=(?P<pending>\d+) root=(?P<root>.+)$"
)
PHASE_PUBLICATION_RE = re.compile(
    r"index_event kind=view_publication plane=views root=(?P<root>.+?) "
    r"outcome=(?P<outcome>\S+) candidates=\d+ blob_puts=(?P<puts>\d+) "
    r"pending_paths=(?P<pending>\d+)(?:\s|$)"
)
EMBED_RE = re.compile(
    r'semantic embedder refresh: root="(?P<root>[^"]+)" .*? files=(?P<files>\d+) '
    r'chunks=(?P<chunks>\d+) batches=(?P<batches>\d+)\b'
)
FUNCTION_PATTERNS = (
    re.compile(r"(?m)^(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_$][A-Za-z0-9_$]*)\b"),
    re.compile(r"(?m)^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\b"),
    re.compile(r"(?m)^(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)\b"),
    re.compile(r"(?m)^func\s+([A-Za-z_][A-Za-z0-9_]*)\s*\("),
)
IGNORED_SYMBOLS = {
    "main",
    "new",
    "default",
    "get",
    "set",
    "run",
    "test",
    "setup",
    "execute",
}


def assert_fresh_storage(path: Path, label: str) -> None:
    if path.exists() and any(path.iterdir()):
        raise SoakError(f"{label} must be fresh and empty: {path}")


def changed_file_count(root: Path, left: str, right: str) -> int:
    output = git_bytes(root, "diff", "--name-only", left, right, "--")
    return len([line for line in output.splitlines() if line])


def changed_target_paths(root: Path, source: str, target: str) -> list[str]:
    output = git_text(root, "diff", "--name-status", "--find-renames", source, target, "--")
    added: list[str] = []
    changed: list[str] = []
    for line in output.splitlines():
        fields = line.split("\t")
        if len(fields) < 2:
            continue
        status = fields[0]
        path = fields[-1]
        if Path(path).suffix.lower() not in {
            ".go",
            ".js",
            ".jsx",
            ".py",
            ".rs",
            ".ts",
            ".tsx",
            ".vue",
        }:
            continue
        lowered = path.lower()
        if any(marker in lowered for marker in ("/test/", "/tests/", "__tests__", ".test.", ".spec.", "fixture")):
            continue
        if status.startswith("A") or status.startswith("R") or status.startswith("C"):
            added.append(path)
        elif status.startswith("M"):
            changed.append(path)
    return sorted(dict.fromkeys(added)) + sorted(dict.fromkeys(changed))


def symbols_in_source(source: str) -> list[str]:
    symbols: list[str] = []
    for pattern in FUNCTION_PATTERNS:
        for match in pattern.finditer(source):
            symbol = match.group(1)
            if len(symbol) < 5 or symbol.lower() in IGNORED_SYMBOLS or symbol in symbols:
                continue
            symbols.append(symbol)
    return symbols


def git_grep_occurrences(root: Path, ref: str, token: str) -> int:
    result = run_checked(
        ["git", "-C", root, "grep", "-n", "-w", "-e", token, ref, "--"],
        allowed=(0, 1),
        timeout_s=120.0,
    )
    return len(result.stdout.splitlines()) if result.returncode == 0 else 0


def probe_has_definition_and_reference_evidence(occurrences: int) -> bool:
    return occurrences >= 2


def git_grep_has(root: Path, ref: str, token: str) -> bool:
    return git_grep_occurrences(root, ref, token) > 0


def find_switch_probe(root: Path, source: str, target: str) -> SwitchProbe:
    for path in changed_target_paths(root, source, target)[:500]:
        result = run_checked(
            ["git", "-C", root, "show", f"{target}:{path}"],
            allowed=(0, 128),
            timeout_s=60.0,
        )
        if result.returncode != 0:
            continue
        text = result.stdout.decode("utf-8", errors="replace")
        for symbol in symbols_in_source(text):
            if not probe_has_definition_and_reference_evidence(
                git_grep_occurrences(root, target, symbol)
            ):
                continue
            if git_grep_has(root, source, symbol):
                continue
            return SwitchProbe(path=path, symbol=symbol, token=symbol)
    raise SoakError(
        f"could not find a top-level function unique to target {target[:12]} from {source[:12]}"
    )


def candidate_history_refs(root: Path, head: str) -> list[tuple[str, str, int]]:
    ancestors = git_text(
        root, "rev-list", "--first-parent", "--max-count=2001", head
    ).splitlines()[1:]
    eligible: list[tuple[str, str, int]] = []
    nearest: tuple[str, str, int] | None = None
    crossed_range = False
    beyond_range = 0
    for sha in ancestors:
        count = changed_file_count(root, head, sha)
        candidate = (sha[:12], sha, count)
        if nearest is None or abs(count - 300) < abs(nearest[2] - 300):
            nearest = candidate
        if 200 <= count <= 400:
            eligible.append(candidate)
        if count > 400:
            crossed_range = True
            beyond_range += 1
        elif crossed_range:
            beyond_range = 0
        # First-parent distance is normally monotonic enough that 100 commits
        # beyond the requested range are sufficient; the 2,000-commit cap is
        # retained for histories with large reverts or vendor drops.
        if eligible and beyond_range >= 100:
            break
    if eligible:
        return sorted(eligible, key=lambda item: (abs(item[2] - 300), item[0]))
    return [nearest] if nearest is not None else []


def choose_anchor(
    root: Path, head: str
) -> tuple[tuple[str, str, int], dict[tuple[str, str], SwitchProbe]]:
    candidates = candidate_history_refs(root, head)
    if not candidates:
        raise SoakError("HEAD has no first-parent ancestor within the 2,000-commit search cap")
    failures: list[str] = []
    for candidate in candidates[:20]:
        label, sha, _ = candidate
        try:
            probes = {
                (head, sha): find_switch_probe(root, head, sha),
                (sha, head): find_switch_probe(root, sha, head),
            }
            return candidate, probes
        except SoakError as error:
            failures.append(f"{label}: {error}")
    raise SoakError(
        "no eligible first-parent commit has bidirectional correctness probes: "
        + "; ".join(failures)
    )


def branch_priority(root: Path, head: str, ref: str) -> tuple[int, int, str]:
    short = ref.removeprefix("refs/heads/").removeprefix("refs/remotes/")
    basename = short.rsplit("/", 1)[-1]
    priorities = {"dev": 1, "main": 1, "master": 1, "next": 1}
    sha = git_text(root, "rev-parse", f"{ref}^{{commit}}")
    distance = changed_file_count(root, head, sha)
    # Prefer a moderate-churn branch over a multi-thousand-file trunk fork; the
    # drill needs a distinct branch, not an artificial machine saturation test.
    return abs(distance - 300), priorities.get(basename, 0), ref


def choose_branch(
    root: Path,
    head: str,
    excluded_sha: str,
) -> tuple[tuple[str, str, int], dict[tuple[str, str], SwitchProbe]]:
    refs = git_text(
        root,
        "for-each-ref",
        "--format=%(refname)",
        "refs/heads",
        "refs/remotes",
    ).splitlines()
    failures: list[str] = []
    for ref in sorted(refs, key=lambda candidate: branch_priority(root, head, candidate)):
        if ref.endswith("/HEAD"):
            continue
        sha = git_text(root, "rev-parse", f"{ref}^{{commit}}")
        if sha in {head, excluded_sha}:
            continue
        try:
            probes = {
                (head, sha): find_switch_probe(root, head, sha),
                (sha, head): find_switch_probe(root, sha, head),
            }
            return (ref, sha, changed_file_count(root, head, sha)), probes
        except SoakError as error:
            failures.append(f"{ref}: {error}")
        if len(failures) == 30:
            break
    raise SoakError("no third branch has bidirectional correctness probes: " + "; ".join(failures))


def search_query_for_token(token: str) -> str:
    """Force the search lane to return an exact token-bearing match."""
    return rf"\b{re.escape(token)}\b"


def search_is_correct(response: Mapping[str, Any], token: str) -> bool:
    if response.get("success") is not True or response.get("status") != "ready":
        return False
    observable = {
        "text": response.get("text"),
        "results": response.get("results"),
    }
    return token.lower() in json.dumps(observable, ensure_ascii=False).lower()


def callgraph_is_correct(response: Mapping[str, Any]) -> bool:
    return response.get("success") is True


def file_log_mark(path: Path) -> int:
    try:
        return path.stat().st_size
    except FileNotFoundError:
        return 0


def file_log_since(path: Path, mark: int) -> str:
    try:
        size = path.stat().st_size
    except FileNotFoundError:
        return ""
    chunks: list[str] = []
    if size < mark:
        rotated = Path(str(path) + ".1")
        if rotated.is_file():
            with rotated.open(encoding="utf-8", errors="replace") as handle:
                handle.seek(min(mark, rotated.stat().st_size))
                chunks.append(handle.read())
        mark = 0
    with path.open(encoding="utf-8", errors="replace") as handle:
        handle.seek(min(mark, size))
        chunks.append(handle.read())
    return "".join(chunks)


def log_metrics(text: str, root: Path) -> tuple[int | None, int | None, int, int]:
    reuse_puts: int | None = None
    publication_puts: int | None = None
    phase_puts: int | None = None
    embed_calls = 0
    embedded_files = 0
    root_texts = {str(root), str(root.resolve())}
    for line in text.splitlines():
        reuse = REUSE_RE.search(line)
        if reuse and reuse.group("root") in root_texts:
            reuse_puts = int(reuse.group("total")) - int(reuse.group("reused"))
        publication = PUBLICATION_RE.search(line)
        if (
            publication
            and publication.group("published") == "true"
            and publication.group("root") in root_texts
        ):
            publication_puts = int(publication.group("puts"))
        phase = PHASE_PUBLICATION_RE.search(line)
        if (
            phase
            and phase.group("outcome") in {"published", "no_op"}
            and phase.group("root") in root_texts
        ):
            phase_puts = (phase_puts or 0) + int(phase.group("puts"))
        embed = EMBED_RE.search(line)
        if embed and embed.group("root") in root_texts:
            embed_calls += int(embed.group("batches"))
            embedded_files += int(embed.group("files"))
    # Prepared publication emits a root-owned phase profile, not the older
    # summary line. Membership deltas are not blob writes (deleting 15 entries
    # can still put zero blobs), so prefer explicit phase counters, summed across
    # the graph publication and subsequent semantic fills in this switch window.
    return (
        reuse_puts,
        phase_puts if phase_puts is not None else publication_puts,
        embed_calls,
        embedded_files,
    )


def publication_is_complete(text: str, root: Path) -> bool:
    """Return whether the latest root-owned publication has no pending paths."""
    root_texts = {str(root), str(root.resolve())}
    latest_pending: int | None = None
    for line in text.splitlines():
        phase = PHASE_PUBLICATION_RE.search(line)
        if phase and phase.group("root") in root_texts:
            latest_pending = int(phase.group("pending"))
    return latest_pending == 0


def _parse_log_timestamp(text: str) -> datetime | None:
    try:
        return datetime.fromisoformat(text.replace("Z", "+00:00"))
    except ValueError:
        return None




def _pending_count(value: Any) -> int:
    if value is None:
        return 0
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, (int, float)):
        return int(value)
    if isinstance(value, (list, tuple, set, dict)):
        return len(value)
    return 0






def contention_metrics(text: str, root: Path) -> dict[str, Any]:
    root_markers = {f"root={root}", f"root={root.resolve()}"}
    build_progress_lines = 0
    tier2_refresh_scheduled: list[str] = []
    tier2_phase_lines: list[str] = []
    tier2_categories: list[str] = []
    for line in text.splitlines():
        root_owned = any(marker in line for marker in root_markers)
        if "index_event " in line and (match := re.search(r"\broot=(\S+)", line)):
            root_owned = any(
                index_event_root_matches(match.group(1), str(candidate))
                for candidate in (root, root.resolve())
            )
        if "index_event kind=build_progress" in line and root_owned:
            build_progress_lines += 1
        if "tier2 refresh scheduled" in line:
            tier2_refresh_scheduled.append(line)
        if "perf tier2 phases" in line and root_owned:
            tier2_phase_lines.append(line)
            category = re.search(r"\bcategory=([^\s]+)", line)
            if category:
                tier2_categories.append(category.group(1))
    parts = [f"build_progress={build_progress_lines}"]
    if tier2_refresh_scheduled:
        parts.append(f"tier2_scheduled={len(tier2_refresh_scheduled)}")
    if tier2_categories:
        parts.append("tier2_phases=" + ",".join(tier2_categories))
    return {
        "build_progress_lines": build_progress_lines,
        "tier2_refresh_scheduled_lines": tier2_refresh_scheduled,
        "tier2_phase_lines": tier2_phase_lines,
        "summary": "none" if parts == ["build_progress=0"] else "; ".join(parts),
    }


def publication_outcome(
    before_generation: str | None,
    after_generation: str | None,
    before_fingerprint: str | None,
    after_fingerprint: str | None,
    expected_fingerprint: str | None,
) -> str:
    if after_generation != before_generation:
        if expected_fingerprint is not None and after_fingerprint != expected_fingerprint:
            return "mismatched"
        return "published"
    if expected_fingerprint is not None and after_fingerprint == expected_fingerprint:
        return "no_op"
    return "missing"


def delta_metrics(before: ProcessSample, after: ProcessSample) -> tuple[float, float]:
    return (
        round(max(0.0, after.cpu_s - before.cpu_s), 3),
        round((after.rss_kib - before.rss_kib) / 1024.0, 3),
    )


def response_observation(response: Mapping[str, Any]) -> dict[str, Any]:
    observation = {
        key: response[key]
        for key in ("success", "status", "code", "message", "result_count")
        if key in response
    }
    if "text" in response:
        text = str(response["text"])
        observation["text"] = text[:2000] + ("…" if len(text) > 2000 else "")
    return observation


def perform_switch(
    *,
    mode: str,
    checkout: Path,
    target_sha: str,
    changed_files: int,
    label: str,
    probe: SwitchProbe,
    client: ToolClient,
    view_dir: Path,
    storage: Path,
    expected_manifest_fingerprint: str | None,
    standalone_pid: int | None = None,
    cold_work_timeout_s: float = 1800.0,
) -> dict[str, Any]:
    before_generation = current_generation(view_dir)
    before_manifest_fingerprint = (
        manifest_fingerprint(view_dir, before_generation)
    )
    before_entries = manifest_entry_count(view_dir, before_generation)
    daemon_subject = standalone_pid is None
    before_pid = find_subc_daemon_pid() if daemon_subject else standalone_pid
    if before_pid is None:
        raise SoakError(f"{mode} has no measurement process")
    before_process = sample_process(before_pid)
    if isinstance(client, NdjsonClient):
        log_path = client.stderr_path
        log_mark = client.log_mark()
    else:
        log_path = storage / "logs" / f"aft-{before_pid}.log"
        log_mark = file_log_mark(log_path)

    def read_log_window() -> str:
        if isinstance(client, NdjsonClient):
            return client.log_since(log_mark)
        return file_log_since(log_path, log_mark)

    started = time.monotonic()
    # The in-process contention column cannot see other processes on the box
    # (a concurrent Docker matrix doubled one run's materialization time while
    # every in-process signal read idle); the host load brackets each row so a
    # loaded machine is visible in the table rather than read as a regression.
    load_at_start = round(os.getloadavg()[0], 2)
    checkout_args = ["checkout", "--quiet", "--detach"]
    checkout_args.append(target_sha)
    git_text(checkout, *checkout_args)

    # Each checkout must finish its publication inside the measured row budget.
    deadline = started + cold_work_timeout_s
    publication_ms: int | None = None
    time_to_correct_ms: int | None = None
    row_close_reason = "timeout"
    readiness_ms: int | None = None
    readiness_error: str | None = None
    query_embedding_calls = 0
    readiness_observations: list[dict[str, Any]] = []
    last_search: dict[str, Any] = {}
    last_callgraph: dict[str, Any] = {}
    contention_log_text: str | None = None
    while time.monotonic() < deadline:
        remaining = max(1.0, deadline - time.monotonic())
        try:
            ready = wait_indexes_ready(client, probe, timeout_s=min(60.0, remaining))
            readiness_ms = round((time.monotonic() - started) * 1000)
            status = ready.get("status", {})
            search_status = status.get("search_index", {})
            semantic_status = status.get("semantic_index", {})
            callgraph_status = status.get("callgraph_store", {})
            states = {
                "search": search_status.get("status"),
                "semantic": semantic_status.get("status"),
                "callgraph": callgraph_status.get("status"),
            }
            previous = (
                {key: readiness_observations[-1][key] for key in states}
                if readiness_observations
                else None
            )
            if states != previous:
                readiness_observations.append({"elapsed_ms": readiness_ms, **states})
        except (SoakError, subprocess.SubprocessError) as error:
            readiness_error = str(error)
            break
        if publication_ms is None:
            generation = current_generation(view_dir)
            if generation is not None and generation != before_generation:
                publication_ms = round((time.monotonic() - started) * 1000)
        last_search = client.tool(
            "search", {"query": search_query_for_token(probe.token), "topK": 20}
        )
        query_embedding_calls += extract_embedding_calls(last_search)
        last_callgraph = client.tool(
            "callgraph",
            {"op": "callers", "path": probe.path, "symbol": probe.symbol},
        )
        answers_correct = search_is_correct(
            last_search, probe.token
        ) and callgraph_is_correct(last_callgraph)
        if answers_correct and time_to_correct_ms is None:
            time_to_correct_ms = round((time.monotonic() - started) * 1000)
        if answers_correct:
            current_log_text = read_log_window()
            if publication_is_complete(current_log_text, checkout):
                row_close_reason = "pending_paths=0"
                contention_log_text = current_log_text
                break
        time.sleep(0.25)

    if contention_log_text is None:
        contention_log_text = read_log_window()
    if publication_ms is None:
        generation = current_generation(view_dir)
        if generation is not None and generation != before_generation:
            publication_ms = round((time.monotonic() - started) * 1000)
    # A ready status after the answer ensures refresh-completion logs have reached
    # the daemon drain before the byte range is read.
    try:
        client.status(timeout_s=300.0)
    except (SoakError, subprocess.SubprocessError) as error:
        readiness_error = readiness_error or str(error)
    time.sleep(0.5)

    after_pid = find_subc_daemon_pid() if daemon_subject else standalone_pid
    pid_changed = after_pid != before_pid
    after_process = sample_process(after_pid) if after_pid is not None else None
    if pid_changed or after_process is None:
        process_cpu_s = None
        process_rss_delta_mb = None
    else:
        process_cpu_s, process_rss_delta_mb = delta_metrics(before_process, after_process)

    if isinstance(client, NdjsonClient):
        log_text = client.log_since(log_mark)
    else:
        log_text = file_log_since(log_path, log_mark)
        if pid_changed and after_pid is not None:
            log_text += file_log_since(storage / "logs" / f"aft-{after_pid}.log", 0)
    reuse_puts, publication_puts, embed_calls, embedded_files = log_metrics(log_text, checkout)
    root_markers = {f"root={checkout}", f"root={checkout.resolve()}"}
    index_events = [
        line
        for line in log_text.splitlines()
        if "index_event " in line and (
            any(marker in line for marker in root_markers)
            or any(
                index_event_root_matches(match.group(1), str(candidate))
                for match in re.finditer(r"\broot=(\S+)", line)
                for candidate in (checkout, checkout.resolve())
            )
        )
    ][-200:]
    after_generation = current_generation(view_dir)
    after_manifest_fingerprint = (
        manifest_fingerprint(view_dir, after_generation)
    )
    after_entries = manifest_entry_count(view_dir, after_generation)
    puts: int | None = None
    puts_source = "not_applicable"
    if publication_puts is not None:
        puts = publication_puts
        puts_source = "publication_log"
    elif reuse_puts is not None:
        puts = reuse_puts
        puts_source = "head_reuse"
    else:
        puts = abs(after_entries - before_entries)
        puts_source = "entries_delta"

    timed_out = time_to_correct_ms is None
    publication = (
        publication_outcome(
            before_generation,
            after_generation,
            before_manifest_fingerprint,
            after_manifest_fingerprint,
            expected_manifest_fingerprint,
        )
    )
    return {
        "mode": mode,
        "switch": label,
        "target": target_sha,
        "changed_files": changed_files,
        "probe": asdict(probe),
        "generation_before": before_generation,
        "generation_after": after_generation,
        "manifest_fingerprint_before": before_manifest_fingerprint,
        "manifest_fingerprint_after": after_manifest_fingerprint,
        "manifest_fingerprint_expected": expected_manifest_fingerprint,
        "publication_ms": publication_ms,
        "puts": puts,
        "puts_source": puts_source,
        "publication_blob_puts": publication_puts,
        "embeds": embed_calls,
        "embedded_files": embedded_files,
        "query_embedding_calls": query_embedding_calls,
        "cpu_s": process_cpu_s,
        "rss_delta_mb": process_rss_delta_mb,
        "measurement_pid_before": before_pid,
        "measurement_pid_after": after_pid,
        "measurement_pid_changed": pid_changed,
        "readiness_ms": readiness_ms,
        "readiness_observations": readiness_observations,
        "index_events": index_events,
        "contention": contention_metrics(contention_log_text, checkout),
        "host_load_1m": {"start": load_at_start, "end": round(os.getloadavg()[0], 2)},
        "readiness_error": readiness_error,
        "time_to_correct_ms": time_to_correct_ms,
        "row_close_reason": row_close_reason,
        "correctness": "timeout" if timed_out else "correct",
        "publication": publication,
        "last_search": response_observation(last_search) if timed_out else None,
        "last_callgraph": response_observation(last_callgraph) if timed_out else None,
    }


def warm_mode(
    *,
    mode: str,
    checkout: Path,
    client: ToolClient,
    storage: Path,
    standalone_pid: int,
    warm_started: float,
    ceiling_s: float,
) -> dict[str, Any]:
    stable = select_stable_symbols(client, checkout, count=1)
    remaining = ceiling_s - (time.monotonic() - warm_started)
    if remaining <= 0:
        raise SoakError(f"{mode} exhausted its cold-work budget before index readiness")
    ready = wait_indexes_ready(client, stable[0], timeout_s=remaining)
    indexes_ready_ms = round((time.monotonic() - warm_started) * 1000)
    remaining = ceiling_s - (time.monotonic() - warm_started)
    if remaining <= 0:
        raise SoakError(
            f"{mode} cold-work readiness timed out; still running: "
            "semantic state unknown; tier-2 dead_code cold pass incomplete"
        )
    if isinstance(client, NdjsonClient):
        log_reader = lambda: client.log_since(0)
    else:
        log_path = storage / "logs" / f"aft-{standalone_pid}.log"
        log_reader = lambda: file_log_since(log_path, 0)
    cold = wait_cold_work_ready(
        client,
        root=checkout,
        storage_root=storage,
        log_reader=log_reader,
        timeout_s=remaining,
    )
    cold_work_ms = round((time.monotonic() - warm_started) * 1000)
    return {
        "ceiling_ms": round(ceiling_s * 1000),
        "storage": str(storage),
        "subject": "standalone_source_checkout",
        "readiness_detail_ms": ready["elapsed_ms"],
        "indexes_ready_ms": indexes_ready_ms,
        "cold_gate_detail_ms": cold["elapsed_ms"],
        "inspect_timeout_retry_count": cold["inspect_timeout_retry_count"],
        "inspect_timeout_retry_elapsed_ms": cold["inspect_timeout_retry_elapsed_ms"],
        "cold_work_ms": cold_work_ms,
        "total_ms": cold_work_ms,
        "measurement_pid": standalone_pid,
    }


def exercise_mode(
    *,
    mode: str,
    checkout: Path,
    head: str,
    transitions: list[tuple[str, str, str, int]],
    probes: Mapping[tuple[str, str], SwitchProbe],
    client: ToolClient,
    view_dir: Path,
    storage: Path,
    standalone_pid: int,
    cold_work_timeout_s: float = 1800.0,
) -> list[dict[str, Any]]:
    if git_text(checkout, "rev-parse", "HEAD") != head:
        raise SoakError(f"{mode} did not start at HEAD")
    rows: list[dict[str, Any]] = []
    known_manifest_fingerprints: dict[str, str] = {}
    initial_fingerprint = manifest_fingerprint(view_dir)
    if initial_fingerprint is not None:
        known_manifest_fingerprints[head] = initial_fingerprint

    for source, target, label, changed_files in transitions:
        if git_text(checkout, "rev-parse", "HEAD") != source:
            raise SoakError(f"{mode} sequence drift before {label}")
        row = perform_switch(
            mode=mode,
            checkout=checkout,
            target_sha=target,
            changed_files=changed_files,
            label=label,
            probe=probes[(source, target)],
            client=client,
            view_dir=view_dir,
            storage=storage,
            expected_manifest_fingerprint=known_manifest_fingerprints.get(target),
            standalone_pid=standalone_pid,
            cold_work_timeout_s=cold_work_timeout_s,
        )
        rows.append(row)
        if row["publication"] in {"published", "no_op"}:
            fingerprint = row["manifest_fingerprint_after"]
            if fingerprint is not None:
                known_manifest_fingerprints[target] = fingerprint
    return rows


def render_table(
    rows: list[dict[str, Any]],
    refs: Mapping[str, Any],
    defects: list[str],
    warmups: Mapping[str, Any],
    previous: Mapping[str, Any] | None,
) -> str:
    lines = [
        "# opencode source-checkout branch-switch drill", "",
        f"Observed at `{refs['observed_at']}` against `{refs['head'][:12]}`.",
        f"Cold work completed in `{warmups['source-checkout']['cold_work_ms']} ms`.", "",
        "CPU and RSS deltas use one standalone subject PID; PID changes are defects.", "",
        "| switch | publication_ms | puts | embeds | cpu_s | rss_delta_mb | correct_ms | contention | load |",
        "|---|---:|---:|---:|---:|---:|---:|---|---|",
    ]
    for row in rows:
        values = (
            row["switch"], row["publication_ms"], row["puts"], row["embeds"], row["cpu_s"],
            row["rss_delta_mb"], row["time_to_correct_ms"], row["contention"]["summary"],
            f"{row['host_load_1m']['start']}→{row['host_load_1m']['end']}",
        )
        lines.append("| " + " | ".join(markdown_cell(value) for value in values) + " |")
    lines.extend(["", "## Defects", "", *(defects or ["None observed."]), ""])
    return "\n".join(lines)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=OPENCODE_ROOT)
    parser.add_argument(
        "--binary",
        type=Path,
        default=Path.home() / ".local" / "share" / "cortexkit" / "bin" / "ck-aft",
    )
    parser.add_argument(
        "--storage",
        type=Path,
        default=(
            Path.home()
            / ".cache"
            / "aft-views-soak"
            / "0f3900af641f5248"
            / "source-checkout-storage"
        ),
        help="isolated storage for the standalone source-checkout subject",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=RESULT_DIR,
        help="directory for JSON, Markdown, and per-arm stderr logs",
    )
    parser.add_argument(
        "--allow-reused-storage",
        action="store_true",
        help="permit non-empty arm storage (invalid for controlled acceptance runs)",
    )
    parser.add_argument(
        "--cold-work-timeout-s",
        type=float,
        default=1800.0,
        help="per-arm budget for indexes, semantic idle, and the first dead-code pass",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    root, scope = resolve_root(args.root)
    if scope != "0f3900af641f5248":
        raise SoakError("the branch drill is restricted to the opencode soak root")
    binary = args.binary.expanduser().resolve()
    storage = args.storage.expanduser().resolve()
    output_dir = args.output_dir.expanduser().resolve()
    if not args.allow_reused_storage:
        assert_fresh_storage(storage, "source-checkout storage")
    output_dir.mkdir(parents=True, exist_ok=True)
    if not binary.is_file() or not binary.stat().st_mode & 0o111:
        raise SoakError(f"placed AFT binary is missing or not executable: {binary}")
    assert_clean_worktree(root, "opencode soak root")
    original_head = git_text(root, "rev-parse", "HEAD")
    original_branch = git_text(root, "symbolic-ref", "--quiet", "--short", "HEAD", allowed=(0, 1))
    original_config = (root / ".cortexkit" / "aft.jsonc").read_bytes()
    anchor, anchor_probes = choose_anchor(root, original_head)
    branch, branch_probes = choose_branch(root, original_head, anchor[1])
    probes = {**anchor_probes, **branch_probes}
    transitions = [
        (original_head, anchor[1], f"HEAD→{anchor[0]}", anchor[2]),
        (anchor[1], original_head, f"{anchor[0]}→HEAD", anchor[2]),
        (original_head, branch[1], f"HEAD→{branch[0]}", branch[2]),
        (branch[1], original_head, f"{branch[0]}→HEAD", branch[2]),
    ]
    health_before = health_snapshot(binary)
    try:
        warm_started = time.monotonic()
        with NdjsonClient(
            binary, root, storage, output_dir / "branch-drill.stderr.log",
            f"branch-drill-source-{int(time.time())}",
        ) as client:
            client.configure(Path.home() / ".config" / "cortexkit" / "aft.jsonc")
            warmup = warm_mode(
                mode="source-checkout", checkout=root, client=client, storage=storage,
                standalone_pid=client.proc.pid, warm_started=warm_started,
                ceiling_s=args.cold_work_timeout_s,
            )
            rows = exercise_mode(
                mode="source-checkout", checkout=root, head=original_head,
                transitions=transitions, probes=probes, client=client,
                view_dir=storage / "views" / scope, storage=storage,
                standalone_pid=client.proc.pid, cold_work_timeout_s=args.cold_work_timeout_s,
            )
    finally:
        current = git_text(root, "rev-parse", "HEAD", allowed=(0, 128))
        current_branch = git_text(root, "symbolic-ref", "--quiet", "--short", "HEAD", allowed=(0, 1))
        if current != original_head or (original_branch and current_branch != original_branch):
            if original_branch:
                git_text(root, "checkout", "--quiet", original_branch)
            else:
                git_text(root, "checkout", "--quiet", "--detach", original_head)
    if (root / ".cortexkit" / "aft.jsonc").read_bytes() != original_config:
        raise SoakError("opencode project config did not restore byte-for-byte")
    assert_clean_worktree(root, "restored opencode soak root")
    health_after = health_snapshot(binary)
    defects = []
    for row in rows:
        if row["readiness_error"]:
            defects.append(f"{row['switch']} readiness failed: {row['readiness_error']}")
            continue
        if row["correctness"] != "correct":
            defects.append(f"{row['switch']} did not return both correct probes after full readiness")
        if row["publication"] in {"missing", "mismatched"}:
            defects.append(f"{row['switch']} did not publish the expected manifest (outcome={row['publication']})")
        if row["measurement_pid_changed"]:
            defects.append(f"{row['switch']} measurement PID changed from {row['measurement_pid_before']} to {row['measurement_pid_after']}")
        if row["target"] == original_head and (row["puts"] != 0 or row["embeds"] != 0):
            defects.append(f"{row['switch']} reused HEAD with puts={row['puts']} and embeds={row['embeds']}")
    refs = {
        "observed_at": utc_now(), "head": original_head,
        "anchor": {"kind": "first_parent_commit", "sha": anchor[1], "changed_files": anchor[2],
                   "requested_range": [200, 400], "in_requested_range": 200 <= anchor[2] <= 400},
        "branch": {"ref": branch[0], "sha": branch[1], "changed_files": branch[2]},
    }
    warmups = {"source-checkout": warmup}
    result = {
        "schema_version": 5, "observed_at": refs["observed_at"], "root": str(root),
        "refs": refs, "measurement_subject": "standalone_source_checkout", "warmups": warmups,
        "health_before": health_before, "health_after": health_after, "rows": rows, "defects": defects,
    }
    json_path = output_dir / "branch-drill.json"
    write_json(json_path, result)
    markdown_path = output_dir / "branch-drill.md"
    markdown_path.write_text(render_table(rows, refs, defects, warmups, None), encoding="utf-8")
    print(f"wrote {json_path} and {markdown_path}")
    for defect in defects:
        print(f"- {defect}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except (SoakError, OSError, subprocess.SubprocessError) as error:
        print(f"views-branch-drill: {error}", file=sys.stderr)
        raise SystemExit(1)
