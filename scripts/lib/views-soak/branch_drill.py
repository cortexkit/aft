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
    assert_clean_worktree,
    current_generation,
    ensure_baseline,
    extract_embedding_calls,
    find_subc_daemon_pid,
    git_bytes,
    git_text,
    health_snapshot,
    manifest_entry_count,
    markdown_cell,
    read_jsonc,
    require_success,
    resolve_root,
    run_checked,
    sample_process,
    select_stable_symbols,
    utc_now,
    wait_callgraph_ready,
    wait_search_ready,
    write_json,
    write_views_off_config,
)


REPO_ROOT = Path(__file__).resolve().parents[3]
RESULT_DIR = REPO_ROOT / "docs" / "investigations" / "views-soak-2026-09"
OPENCODE_ROOT = Path.home() / "Work" / "OSS" / "opencode"
REUSE_RE = re.compile(
    r"content-addressed view HEAD reuse (?P<reused>\d+)/(?P<total>\d+) for (?P<root>.+)$"
)
PUBLICATION_RE = re.compile(
    r"content-addressed view publication published=(?P<published>true|false) "
    r"blob_puts=(?P<puts>\d+) pending_paths=(?P<pending>\d+)"
)
EMBED_RE = re.compile(
    r'semantic embedder refresh: root="(?P<root>[^"]+)" .*? batches=(?P<batches>\d+)\b'
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


def git_grep_has(root: Path, ref: str, token: str) -> bool:
    result = run_checked(
        ["git", "-C", root, "grep", "-w", "-e", token, ref, "--"],
        allowed=(0, 1),
        timeout_s=120.0,
    )
    return result.returncode == 0


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
            if not git_grep_has(root, target, symbol):
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


def branch_priority(ref: str) -> tuple[int, str]:
    short = ref.removeprefix("refs/heads/").removeprefix("refs/remotes/")
    basename = short.rsplit("/", 1)[-1]
    priorities = {"dev": 0, "main": 1, "master": 2, "next": 3}
    return priorities.get(basename, 10), ref


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
    for ref in sorted(refs, key=branch_priority):
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


def settle_after_correct(client: NdjsonClient, timeout_s: float = 60.0) -> None:
    deadline = time.monotonic() + timeout_s
    quiet_since = time.monotonic()
    previous_size = client.log_mark()
    while time.monotonic() < deadline:
        status = client.status()
        semantic = status.get("semantic_index")
        refreshing = semantic.get("refreshing_count", 0) if isinstance(semantic, Mapping) else 0
        size = client.log_mark()
        if size != previous_size or refreshing:
            quiet_since = time.monotonic()
            previous_size = size
        if not refreshing and time.monotonic() - quiet_since >= 1.0:
            return
        time.sleep(0.1)
    raise SoakError("AFT did not become quiet after a correct branch-switch answer")


def log_metrics(text: str, root: Path) -> tuple[int | None, int | None, int]:
    reuse_puts: int | None = None
    publication_puts: int | None = None
    embed_calls = 0
    root_texts = {str(root), str(root.resolve())}
    for line in text.splitlines():
        reuse = REUSE_RE.search(line)
        if reuse and reuse.group("root") in root_texts:
            reuse_puts = int(reuse.group("total")) - int(reuse.group("reused"))
        publication = PUBLICATION_RE.search(line)
        if publication and publication.group("published") == "true":
            publication_puts = int(publication.group("puts"))
        embed = EMBED_RE.search(line)
        if embed and embed.group("root") in root_texts:
            embed_calls += int(embed.group("batches"))
    return reuse_puts, publication_puts, embed_calls


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
    client: NdjsonClient,
    views_on: bool,
    view_dir: Path,
    daemon_pid: int,
) -> dict[str, Any]:
    before_generation = current_generation(view_dir) if views_on else None
    before_entries = manifest_entry_count(view_dir, before_generation) if views_on else 0
    before_process = sample_process(client.proc.pid)
    before_daemon = sample_process(daemon_pid)
    log_mark = client.log_mark()
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
    query_embedding_calls = 0
    last_search: dict[str, Any] = {}
    last_callgraph: dict[str, Any] = {}
    next_probe_at = started
    while time.monotonic() < deadline:
        now = time.monotonic()
        if views_on and publication_ms is None:
            generation = current_generation(view_dir)
            if generation is not None and generation != before_generation:
                publication_ms = round((now - started) * 1000)
        if time_to_correct_ms is None and now >= next_probe_at:
            last_search = client.tool("search", {"query": probe.token, "topK": 20})
            query_embedding_calls += extract_embedding_calls(last_search)
            last_callgraph = client.tool(
                "callgraph",
                {"op": "callers", "filePath": probe.path, "symbol": probe.symbol},
            )
            if search_is_correct(last_search, probe.token) and callgraph_is_correct(last_callgraph):
                time_to_correct_ms = round((time.monotonic() - started) * 1000)
            next_probe_at = time.monotonic() + 0.15
        if time_to_correct_ms is not None and (not views_on or publication_ms is not None):
            break
        time.sleep(0.05)
    timed_out = time_to_correct_ms is None
    publication_missing = views_on and publication_ms is None
    settle_error: str | None = None
    try:
        settle_after_correct(client)
    except SoakError as error:
        settle_error = str(error)
    after_process = sample_process(client.proc.pid)
    after_daemon = sample_process(daemon_pid)
    process_cpu_s, process_rss_delta_mb = delta_metrics(before_process, after_process)
    daemon_cpu_s, daemon_rss_delta_mb = delta_metrics(before_daemon, after_daemon)
    log_text = client.log_since(log_mark)
    reuse_puts, publication_puts, embed_calls = log_metrics(log_text, checkout)
    after_generation = current_generation(view_dir) if views_on else None
    after_entries = manifest_entry_count(view_dir, after_generation) if views_on else 0
    puts: int | None = None
    puts_source = "not_applicable"
    if views_on:
        if reuse_puts is not None:
            puts = reuse_puts
            puts_source = "head_reuse"
        elif publication_puts is not None:
            puts = publication_puts
            puts_source = "publication_log"
        else:
            puts = abs(after_entries - before_entries)
            puts_source = "entries_delta"

    return {
        "mode": mode,
        "switch": label,
        "target": target_sha,
        "changed_files": changed_files,
        "probe": asdict(probe),
        "generation_before": before_generation,
        "generation_after": after_generation,
        "publication_ms": publication_ms,
        "puts": puts,
        "puts_source": puts_source,
        "publication_blob_puts": publication_puts,
        "embeds": embed_calls,
        "query_embedding_calls": query_embedding_calls,
        "cpu_s": process_cpu_s,
        "rss_delta_mb": process_rss_delta_mb,
        "system_daemon_cpu_s": daemon_cpu_s,
        "system_daemon_rss_delta_mb": daemon_rss_delta_mb,
        "time_to_correct_ms": time_to_correct_ms,
        "correctness": "timeout" if timed_out else "correct",
        "publication": "timeout" if publication_missing else ("published" if views_on else "not_applicable"),
        "settle_error": settle_error,
        "last_search": response_observation(last_search) if timed_out else None,
        "last_callgraph": response_observation(last_callgraph) if timed_out else None,
    }


def wait_initial_view(client: NdjsonClient, view_dir: Path, timeout_s: float = 300.0) -> None:
    deadline = time.monotonic() + timeout_s
    last: dict[str, Any] = {}
    while time.monotonic() < deadline:
        last = client.status()
        views = last.get("views")
        if (
            isinstance(views, Mapping)
            and views.get("generation", 0)
            and current_generation(view_dir) is not None
        ):
            return
        time.sleep(0.1)
    raise SoakError(f"initial views publication did not settle: {last}")


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
    daemon_pid: int,
) -> tuple[list[dict[str, Any]], int]:
    cache_dir = Path.home() / ".cache" / "aft-views-soak" / scope
    user_config = Path.home() / ".config" / "cortexkit" / "aft.jsonc"
    view_dir = storage / "views" / scope
    if git_text(checkout, "rev-parse", "HEAD") != head:
        raise SoakError(f"{mode} did not start at HEAD")
    stderr_path = cache_dir / f"branch-drill-{mode}.stderr.log"
    rows: list[dict[str, Any]] = []
    measurement_pid = 0
    with NdjsonClient(binary, checkout, storage, stderr_path, f"views-branch-{mode}") as client:
        measurement_pid = client.proc.pid
        client.configure(user_config)
        wait_search_ready(client)
        stable = select_stable_symbols(client, checkout, count=1)
        wait_callgraph_ready(client, stable[0])
        if views_on:
            wait_initial_view(client, view_dir)
        for source, target, label, changed_files in transitions:
            if git_text(checkout, "rev-parse", "HEAD") != source:
                raise SoakError(f"{mode} sequence drift before {label}")
            rows.append(
                perform_switch(
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
                    daemon_pid=daemon_pid,
                )
            )
    return rows, measurement_pid


def render_table(rows: list[dict[str, Any]], refs: Mapping[str, Any], defects: list[str]) -> str:
    by_mode = {(row["mode"], row["switch"]): row for row in rows}
    switches = [row["switch"] for row in rows if row["mode"] == "views-on"]
    lines = [
        "# opencode views branch-switch drill",
        "",
        f"Observed at `{refs['observed_at']}` against `{refs['head'][:12]}`.",
        "",
        f"- A: first-parent commit `{refs['anchor']['sha'][:12]}` ({refs['anchor']['changed_files']} changed files)",
        f"- B: branch `{refs['branch']['ref']}` (`{refs['branch']['sha'][:12]}`, {refs['branch']['changed_files']} changed files)",
        f"- Standalone measurement PIDs: views-on `{refs['measurement_pids']['views-on']}`, views-off `{refs['measurement_pids']['views-off']}`",
        f"- Running subc daemon PID (sampled separately in JSON): `{refs['system_daemon_pid']}`",
        "",
        "`cpu_s` and `rss_delta_mb` measure the placed standalone AFT process that owns each drill watcher; the running subc daemon deltas are retained in the JSON rows.",
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
            on["cpu_s"],
            on["rss_delta_mb"],
            "timeout" if on["time_to_correct_ms"] is None else on["time_to_correct_ms"],
            "—" if off["publication_ms"] is None else off["publication_ms"],
            "—" if off["puts"] is None else off["puts"],
            off["embeds"],
            off["cpu_s"],
            off["rss_delta_mb"],
            "timeout" if off["time_to_correct_ms"] is None else off["time_to_correct_ms"],
        )
        lines.append("| " + " | ".join(markdown_cell(value) for value in values) + " |")
    lines.extend(["", "## Defects", ""])
    if defects:
        lines.extend(f"- {defect}" for defect in defects)
    else:
        lines.append("No switch-back reuse defect observed.")
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
        default=Path.home() / ".local" / "share" / "cortexkit" / "aft",
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
    baseline = ensure_baseline(root, scope, original_head)
    daemon_pid = find_subc_daemon_pid()
    health_before = health_snapshot(binary)
    daemon_before = sample_process(daemon_pid)
    rows: list[dict[str, Any]] = []
    measurement_pids: dict[str, int] = {}

    try:
        on_rows, measurement_pids["views-on"] = run_mode(
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
            daemon_pid=daemon_pid,
        )
        rows.extend(on_rows)

        git_text(baseline, "checkout", "--quiet", "--detach", "--force", original_head)
        write_views_off_config(root, baseline)
        off_rows, measurement_pids["views-off"] = run_mode(
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
            daemon_pid=daemon_pid,
        )
        rows.extend(off_rows)
    finally:
        current = git_text(root, "rev-parse", "HEAD", allowed=(0, 128))
        if current != original_head or (original_branch and git_text(root, "symbolic-ref", "--quiet", "--short", "HEAD", allowed=(0, 1)) != original_branch):
            if original_branch:
                git_text(root, "checkout", "--quiet", original_branch)
            else:
                git_text(root, "checkout", "--quiet", "--detach", original_head)
        git_text(baseline, "checkout", "--quiet", "--detach", "--force", original_head)
        write_views_off_config(root, baseline)

    if (root / ".cortexkit" / "aft.jsonc").read_bytes() != original_config:
        raise SoakError("opencode project config did not restore byte-for-byte")
    assert_clean_worktree(root, "restored opencode soak root")
    daemon_after = sample_process(daemon_pid)
    daemon_cpu_s, daemon_rss_delta_mb = delta_metrics(daemon_before, daemon_after)
    health_after = health_snapshot(binary)

    defects = []
    for row in rows:
        if row["correctness"] != "correct":
            defects.append(
                f"{row['mode']} {row['switch']} did not return both correct probes within 300 seconds"
            )
        if row["mode"] == "views-on" and row["publication"] != "published":
            defects.append(f"{row['switch']} did not publish a new pointer generation")
        if row["settle_error"]:
            defects.append(f"{row['mode']} {row['switch']} did not settle: {row['settle_error']}")
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
        "measurement_pids": measurement_pids,
        "system_daemon_pid": daemon_pid,
    }
    result = {
        "schema_version": 1,
        "observed_at": observed_at,
        "root": str(root),
        "baseline": str(baseline),
        "refs": refs,
        "health_before": health_before,
        "health_after": health_after,
        "system_daemon_total": {
            "cpu_s": daemon_cpu_s,
            "rss_delta_mb": daemon_rss_delta_mb,
        },
        "rows": rows,
        "defects": defects,
    }
    json_path = RESULT_DIR / "branch-drill.json"
    markdown_path = RESULT_DIR / "branch-drill.md"
    write_json(json_path, result)
    markdown_path.parent.mkdir(parents=True, exist_ok=True)
    markdown_path.write_text(render_table(rows, refs, defects), encoding="utf-8")
    print(f"wrote {json_path}")
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
