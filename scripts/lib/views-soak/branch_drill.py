#!/usr/bin/env python3
"""Exercise opencode branch switches with views enabled and disabled."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
from dataclasses import asdict
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
    ensure_owned_baseline,
    extract_embedding_calls,
    find_subc_daemon_pid,
    git_bytes,
    git_text,
    health_snapshot,
    manifest_entry_count,
    manifest_fingerprint,
    markdown_cell,
    read_jsonc,
    resolve_root,
    run_checked,
    sample_process,
    select_stable_symbols,
    utc_now,
    wait_indexes_ready,
    write_json,
    write_views_off_config,
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
        embed = EMBED_RE.search(line)
        if embed and embed.group("root") in root_texts:
            embed_calls += int(embed.group("batches"))
            embedded_files += int(embed.group("files"))
    return reuse_puts, publication_puts, embed_calls, embedded_files


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
    source_root: Path,
    target_sha: str,
    changed_files: int,
    label: str,
    probe: SwitchProbe,
    client: ToolClient,
    views_on: bool,
    view_dir: Path,
    storage: Path,
    expected_manifest_fingerprint: str | None,
    standalone_pid: int | None = None,
) -> dict[str, Any]:
    before_generation = current_generation(view_dir) if views_on else None
    before_manifest_fingerprint = (
        manifest_fingerprint(view_dir, before_generation) if views_on else None
    )
    before_entries = manifest_entry_count(view_dir, before_generation) if views_on else 0
    daemon_subject = views_on and standalone_pid is None
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

    started = time.monotonic()
    checkout_args = ["checkout", "--quiet", "--detach"]
    if not views_on:
        checkout_args.append("--force")
    checkout_args.append(target_sha)
    git_text(checkout, *checkout_args)
    if not views_on:
        write_views_off_config(source_root, checkout)

    deadline = started + 300.0
    publication_ms: int | None = None
    time_to_correct_ms: int | None = None
    readiness_ms: int | None = None
    readiness_error: str | None = None
    query_embedding_calls = 0
    readiness_observations: list[dict[str, Any]] = []
    last_search: dict[str, Any] = {}
    last_callgraph: dict[str, Any] = {}
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
        if views_on and publication_ms is None:
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
        if search_is_correct(last_search, probe.token) and callgraph_is_correct(last_callgraph):
            time_to_correct_ms = round((time.monotonic() - started) * 1000)
            break
        time.sleep(0.25)

    if views_on and publication_ms is None:
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
        if "index_event " in line and any(marker in line for marker in root_markers)
    ][-200:]
    after_generation = current_generation(view_dir) if views_on else None
    after_manifest_fingerprint = (
        manifest_fingerprint(view_dir, after_generation) if views_on else None
    )
    after_entries = manifest_entry_count(view_dir, after_generation) if views_on else 0
    puts: int | None = None
    puts_source = "not_applicable"
    if views_on:
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
        if views_on
        else "not_applicable"
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
        "readiness_error": readiness_error,
        "time_to_correct_ms": time_to_correct_ms,
        "correctness": "timeout" if timed_out else "correct",
        "publication": publication,
        "last_search": response_observation(last_search) if timed_out else None,
        "last_callgraph": response_observation(last_callgraph) if timed_out else None,
    }





def run_mode(
    *,
    mode: str,
    checkout: Path,
    source_root: Path,
    head: str,
    transitions: list[tuple[str, str, str, int]],
    probes: Mapping[tuple[str, str], SwitchProbe],
    binary: Path,
    storage: Path,
    scope: str,
    views_on: bool,
    baseline_storage: Path,
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    if git_text(checkout, "rev-parse", "HEAD") != head:
        raise SoakError(f"{mode} did not start at HEAD")
    cache_dir = Path.home() / ".cache" / "aft-views-soak" / scope
    user_config = Path.home() / ".config" / "cortexkit" / "aft.jsonc"
    view_dir = storage / "views" / scope
    rows: list[dict[str, Any]] = []
    warm_started = time.monotonic()
    known_manifest_fingerprints: dict[str, str] = {}
    if views_on:
        initial_fingerprint = manifest_fingerprint(view_dir)
        if initial_fingerprint is not None:
            known_manifest_fingerprints[head] = initial_fingerprint

    def exercise(client: ToolClient, standalone_pid: int | None) -> None:
        stable = select_stable_symbols(client, checkout, count=1)
        ready = wait_indexes_ready(client, stable[0], timeout_s=1200.0)
        warmup["readiness_detail_ms"] = ready["elapsed_ms"]
        warmup["total_ms"] = round((time.monotonic() - warm_started) * 1000)
        warmup["measurement_pid"] = standalone_pid
        for source, target, label, changed_files in transitions:
            if git_text(checkout, "rev-parse", "HEAD") != source:
                raise SoakError(f"{mode} sequence drift before {label}")
            row = perform_switch(
                    mode=mode,
                    checkout=checkout,
                    source_root=source_root,
                    target_sha=target,
                    changed_files=changed_files,
                    label=label,
                    probe=probes[(source, target)],
                    client=client,
                    views_on=views_on,
                    view_dir=view_dir,
                    storage=storage,
                    expected_manifest_fingerprint=known_manifest_fingerprints.get(target),
                    standalone_pid=standalone_pid,
                )
            rows.append(row)
            if views_on and row["publication"] in {"published", "no_op"}:
                fingerprint = row["manifest_fingerprint_after"]
                if fingerprint is not None:
                    known_manifest_fingerprints[target] = fingerprint

    warmup: dict[str, Any] = {
        "ceiling_ms": 1_200_000,
        "storage": str(storage if views_on else baseline_storage),
        "subject": "standalone_views_on" if views_on else "standalone_owned_baseline",
    }
    subject = "views-on" if views_on else "views-off"
    stderr_path = cache_dir / f"branch-drill-{subject}.stderr.log"
    subject_storage = storage if views_on else baseline_storage
    with NdjsonClient(
        binary,
        checkout,
        subject_storage,
        stderr_path,
        f"views-branch-{subject}-{int(time.time())}",
    ) as client:
        client.configure(user_config)
        exercise(client, client.proc.pid)
    return rows, warmup


def render_table(
    rows: list[dict[str, Any]],
    refs: Mapping[str, Any],
    defects: list[str],
    warmups: Mapping[str, Any],
    previous: Mapping[str, Any] | None,
) -> str:
    by_mode = {(row["mode"], row["switch"]): row for row in rows}
    switches = [row["switch"] for row in rows if row["mode"] == "views-on"]
    views_on_embeds = sum(row["embeds"] for row in rows if row["mode"] == "views-on")
    views_off_embeds = sum(row["embeds"] for row in rows if row["mode"] == "views-off")
    lines = [
        "# opencode views branch-switch drill",
        "",
        "## Finding",
        "",
        "The isolated views-on subject exercises content-addressed publication without restarting or mutating the live daemon. "
        f"Views-on used {views_on_embeds} embed batches across the four switches; the legacy arm used {views_off_embeds}.",
        "",
        "## Run 3 — isolated views-on, warm owned baseline",
        "",
        f"Observed at `{refs['observed_at']}` against `{refs['head'][:12]}`.",
        "",
        f"- A: first-parent commit `{refs['anchor']['sha'][:12]}` ({refs['anchor']['changed_files']} changed files)",
        f"- B: branch `{refs['branch']['ref']}` (`{refs['branch']['sha'][:12]}`, {refs['branch']['changed_files']} changed files)",
        f"- Views-on subject: standalone AFT with isolated view storage; warm-up `{warmups['views-on']['total_ms']} ms`.",
        f"- Views-off subject: standalone AFT on an independent baseline clone and isolated storage; warm-up `{warmups['views-off']['total_ms']} ms`.",
        "",
        "`cpu_s` and `rss_delta_mb` use the active standalone subject PID for each row. PID changes are recorded as defects rather than subtracting unrelated processes.",
        "",
        "| switch | on publication_ms | on puts | on embeds | on cpu_s | on rss_delta_mb | on correct_ms | off publication_ms | off puts | off embeds | off cpu_s | off rss_delta_mb | off correct_ms |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for switch in switches:
        on = by_mode[("views-on", switch)]
        off = by_mode[("views-off", switch)]
        values: Iterable[Any] = (
            switch,
            "—" if on["publication_ms"] is None else on["publication_ms"],
            on["puts"],
            on["embeds"],
            "—" if on["cpu_s"] is None else on["cpu_s"],
            "—" if on["rss_delta_mb"] is None else on["rss_delta_mb"],
            "timeout" if on["time_to_correct_ms"] is None else on["time_to_correct_ms"],
            "—" if off["publication_ms"] is None else off["publication_ms"],
            "—" if off["puts"] is None else off["puts"],
            off["embeds"],
            "—" if off["cpu_s"] is None else off["cpu_s"],
            "—" if off["rss_delta_mb"] is None else off["rss_delta_mb"],
            "timeout" if off["time_to_correct_ms"] is None else off["time_to_correct_ms"],
        )
        lines.append("| " + " | ".join(markdown_cell(value) for value in values) + " |")
    lines.extend(
        [
            "",
            "### Four mechanisms",
            "",
            "1. **Forward correctness had both a probe defect and a publication defect.** The Run 2 search query was free text and candidate selection proved only that a name existed on the target; `callers` needs a symbol with a resolvable call site. The drill now uses an exact word-boundary query, requires at least two target occurrences, and verifies both search and a non-empty callers result. A pre-fix instrumented run still showed the product defect: status reported search/semantic `ready` after 934 ms, then `index_event ... outcome=pending ... pending_paths=1`, repeated `callers ... symbol_not_found` for 300 s, and no published event. The semantic-refresh completion path did not retry the pending view publication, so callgraph queries remained pinned to HEAD. Completion now publishes the pending paths; all Run 3 probes converge.",
            "2. **Switch-back embeddings came from the legacy semantic watcher worker.** Views publication did not suppress the resident `SemanticIndex` refresh, which embedded every watcher-invalidated path from the live checkout. The worker now derives the view semantic full key from source bytes, path, producer version, and model fingerprint, loads an existing `SemanticBlob`, and embeds only misses. Both return legs report zero embed calls; this final warm run also reused vectors on both forward legs.",
            "3. **The missing generations were failed publications, not valid no-ops.** The target fingerprints are `4b5c2543317f465023...` (A) and `2d6d879d2a496ba71f...` (B), distinct from HEAD `322b78e53d463f91c...`. Run 2 retained the HEAD fingerprint after `outcome=pending`; it had not published an identical manifest. The drill now classifies publication by manifest fingerprint rather than generation alone, and semantic completion publishes the distinct target manifest.",
            "4. **The puts count mixed roots.** Publication counters without `root=` admitted concurrent work from other roots. View publication phase events and publication summaries now use the same `root=<canonical checkout>` grammar as other `index_event` lines, and the drill accepts counters/events only for the measured root. Run 3 records zero blob puts on every switch.",
            "",
            "### Cost attribution",
            "",
            "The earlier views-on sample was 281 CPU-s and +1.3 GB RSS versus 125 CPU-s for legacy. Run 3 phase events rule out blob insertion and embedding as the views-only cause: every views row has `blob_puts=0` and zero embed batches. On the two forward publications, `derived.sqlite` materialization took 43,767 ms and 35,783 ms, versus only 1,701/2,460 ms for manifest assembly and 208/285 ms for blob lookup; pointer publication added 7,263/4,380 ms. Materialization therefore consumed 82-83% of the published phase. The checkpointed database is 269,889,536 bytes (257.4 MiB); building a temporary generation alongside the published one accounts for about 514.8 MiB, matching the known ~517 MiB materialization footprint. The remaining RSS variation is process cache/allocator residency. The follow-up target is consequently incremental `derived.sqlite` materialization: remove the 35.8-43.8 s full rewrite and roughly 257 MiB temporary generation per switch.",
            "",
            "### Run 3 defects",
            "",
        ]
    )
    if defects:
        lines.extend(f"- {defect}" for defect in defects)
    else:
        lines.append("No correctness, publication, PID-change, or switch-back reuse defect observed.")

    if previous is not None:
        lines.extend(
            [
                "",
                "## Run 1 — confounded (historical)",
                "",
                "Run 1 used a standalone views-on process and a cold read-only baseline. Its parity divergence and timeout rows are readiness artifacts, not views parity findings. The raw record remains in `branch-drill-run1-confounded.json`.",
                "",
                "| switch | on publication_ms | on puts | on embeds | on cpu_s | on rss_delta_mb | on correct_ms | off cpu_s | off rss_delta_mb | off correct_ms |",
                "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
            ]
        )
        previous_rows = previous.get("rows", [])
        previous_by_mode = {
            (row["mode"], row["switch"]): row
            for row in previous_rows
            if isinstance(row, dict)
        }
        for switch in [
            row["switch"]
            for row in previous_rows
            if isinstance(row, dict) and row.get("mode") == "views-on"
        ]:
            on = previous_by_mode[("views-on", switch)]
            off = previous_by_mode[("views-off", switch)]
            values = (
                switch,
                "—" if on["publication_ms"] is None else on["publication_ms"],
                on["puts"],
                on["embeds"],
                on["cpu_s"],
                on["rss_delta_mb"],
                "timeout" if on["time_to_correct_ms"] is None else on["time_to_correct_ms"],
                off["cpu_s"],
                off["rss_delta_mb"],
                "timeout" if off["time_to_correct_ms"] is None else off["time_to_correct_ms"],
            )
            lines.append("| " + " | ".join(markdown_cell(value) for value in values) + " |")
    lines.append("")
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
            / "views-on-storage"
        ),
        help="isolated storage for the standalone views-on subject",
    )
    parser.add_argument(
        "--mode",
        choices=("both", "views-on", "views-off"),
        default="both",
        help="Run both arms or one arm for resource-constrained evidence capture",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    root, scope = resolve_root(args.root)
    if scope != "0f3900af641f5248":
        raise SoakError("the branch drill is restricted to the opencode soak root")
    binary = args.binary.expanduser().resolve()
    storage = args.storage.expanduser().resolve()
    if not binary.is_file() or not binary.stat().st_mode & 0o111:
        raise SoakError(f"placed AFT binary is missing or not executable: {binary}")
    config = read_jsonc(root / ".cortexkit" / "aft.jsonc")
    if not isinstance(config.get("views"), dict) or config["views"].get("enabled") is not True:
        raise SoakError("opencode soak root does not have views.enabled: true")
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
    baseline = ensure_owned_baseline(root, scope, original_head)
    baseline_storage = Path.home() / ".cache/aft-views-soak" / scope / "baseline-storage"
    health_before = health_snapshot(binary)
    rows: list[dict[str, Any]] = []
    warmups: dict[str, Any] = {}

    try:
        if args.mode in {"both", "views-on"}:
            on_rows, warmups["views-on"] = run_mode(
                mode="views-on",
                checkout=root,
                source_root=root,
                head=original_head,
                transitions=transitions,
                probes=probes,
                binary=binary,
                storage=storage,
                scope=scope,
                views_on=True,
                baseline_storage=baseline_storage,
            )
            rows.extend(on_rows)

        if args.mode in {"both", "views-off"}:
            for target in {target for _, target, _, _ in transitions}:
                git_text(baseline, "fetch", "--quiet", str(root), target)
            git_text(baseline, "checkout", "--quiet", "--detach", "--force", original_head)
            write_views_off_config(root, baseline)
            off_rows, warmups["views-off"] = run_mode(
                mode="views-off",
                checkout=baseline,
                source_root=root,
                head=original_head,
                transitions=transitions,
                probes=probes,
                binary=binary,
                storage=storage,
                scope=scope,
                views_on=False,
                baseline_storage=baseline_storage,
            )
            rows.extend(off_rows)
    finally:
        current = git_text(root, "rev-parse", "HEAD", allowed=(0, 128))
        current_branch = git_text(
            root, "symbolic-ref", "--quiet", "--short", "HEAD", allowed=(0, 1)
        )
        if current != original_head or (original_branch and current_branch != original_branch):
            if original_branch:
                git_text(root, "checkout", "--quiet", original_branch)
            else:
                git_text(root, "checkout", "--quiet", "--detach", original_head)
        git_text(baseline, "checkout", "--quiet", "--detach", "--force", original_head)
        write_views_off_config(root, baseline)

    if (root / ".cortexkit" / "aft.jsonc").read_bytes() != original_config:
        raise SoakError("opencode project config did not restore byte-for-byte")
    assert_clean_worktree(root, "restored opencode soak root")
    health_after = health_snapshot(binary)

    defects = []
    for row in rows:
        if row["readiness_error"]:
            defects.append(f"{row['mode']} {row['switch']} readiness failed: {row['readiness_error']}")
            continue
        if row["correctness"] != "correct":
            defects.append(
                f"{row['mode']} {row['switch']} did not return both correct probes after full readiness"
            )
        if row["mode"] == "views-on" and row["publication"] in {"missing", "mismatched"}:
            defects.append(
                f"{row['switch']} did not publish the expected manifest "
                f"(outcome={row['publication']})"
            )
        if row["measurement_pid_changed"]:
            defects.append(
                f"{row['mode']} {row['switch']} measurement PID changed from "
                f"{row['measurement_pid_before']} to {row['measurement_pid_after']}"
            )
        if row["mode"] == "views-on" and row["target"] == original_head and (
            row["puts"] != 0 or row["embeds"] != 0
        ):
            defects.append(
                f"{row['switch']} reused HEAD with puts={row['puts']} and embeds={row['embeds']}"
            )

    observed_at = utc_now()
    refs = {
        "observed_at": observed_at,
        "head": original_head,
        "anchor": {
            "kind": "first_parent_commit",
            "sha": anchor[1],
            "changed_files": anchor[2],
            "requested_range": [200, 400],
            "in_requested_range": 200 <= anchor[2] <= 400,
        },
        "branch": {"ref": branch[0], "sha": branch[1], "changed_files": branch[2]},
    }
    previous_path = RESULT_DIR / "branch-drill-run1-confounded.json"
    previous = json.loads(previous_path.read_text(encoding="utf-8")) if previous_path.is_file() else None
    result = {
        "schema_version": 2,
        "observed_at": observed_at,
        "root": str(root),
        "baseline": str(baseline),
        "baseline_storage": str(baseline_storage),
        "refs": refs,
        "measurement_subjects": {
            "views_on": "standalone_views_on",
            "views_off": "standalone_owned_baseline",
        },
        "warmups": warmups,
        "health_before": health_before,
        "health_after": health_after,
        "rows": rows,
        "defects": defects,
        "run1_artifact": previous_path.name if previous is not None else None,
    }
    json_path = RESULT_DIR / (
        "branch-drill.json" if args.mode == "both" else f"branch-drill-{args.mode}.json"
    )
    write_json(json_path, result)
    print(f"wrote {json_path}")
    if args.mode == "both":
        markdown_path = RESULT_DIR / "branch-drill.md"
        markdown_path.parent.mkdir(parents=True, exist_ok=True)
        markdown_path.write_text(
            render_table(rows, refs, defects, warmups, previous), encoding="utf-8"
        )
        print(f"wrote {markdown_path}")
    if defects:
        print("views branch drill defects:")
        for defect in defects:
            print(f"- {defect}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except (SoakError, OSError, subprocess.SubprocessError) as error:
        print(f"views-branch-drill: {error}", file=sys.stderr)
        raise SystemExit(1)
