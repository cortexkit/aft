#!/usr/bin/env python3
"""Parity and storage-accounting probe for a configured views-soak root."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Mapping

from common import (
    NdjsonClient,
    SoakError,
    SubcClient,
    ToolClient,
    StableSymbol,
    append_json_line,
    canonical_output,
    ensure_owned_baseline,
    first_differing_line,
    git_bytes,
    git_text,
    health_snapshot,
    latest_json_line,
    markdown_cell,
    read_jsonc,
    resolve_root,
    resolve_subc_connection_file,
    select_stable_symbols,
    utc_now,
    view_accounting,
    wait_indexes_ready,
)


REPO_ROOT = Path(__file__).resolve().parents[3]
RESULT_DIR = REPO_ROOT / "docs" / "investigations" / "views-soak-2026-09"
STRING_PHRASE_RE = re.compile(r"(?P<quote>['\"])(?P<text>[A-Za-z][^'\"\r\n]{7,60})(?P=quote)")


def quoted_phrase(root: Path, symbol: StableSymbol) -> str:
    source = git_bytes(root, "show", f"HEAD:{symbol.path}").decode("utf-8", errors="replace")
    for match in STRING_PHRASE_RE.finditer(source):
        phrase = " ".join(match.group("text").split())
        if len(phrase.split()) >= 2 and not any(char in phrase for char in "{}$\\/"):
            return f'"{phrase}"'
    return '"not found"'


def outline_directories(symbols: list[StableSymbol]) -> list[str]:
    directories: list[str] = []
    for symbol in symbols:
        parent = str(Path(symbol.path).parent).replace("\\", "/")
        if parent == ".":
            continue
        if parent not in directories:
            directories.append(parent)
        if len(directories) == 2:
            return directories
    top_levels = []
    for symbol in symbols:
        first = Path(symbol.path).parts[0]
        if first not in top_levels:
            top_levels.append(first)
    for candidate in top_levels:
        if candidate not in directories:
            directories.append(candidate)
        if len(directories) == 2:
            break
    if len(directories) != 2:
        raise SoakError("could not select two distinct directories for outline parity")
    return directories


def query_set(root: Path, symbols: list[StableSymbol]) -> list[dict[str, Any]]:
    queries: list[dict[str, Any]] = []
    for symbol in symbols:
        for operation in ("callers", "impact", "call_tree"):
            queries.append(
                {
                    "tool": "callgraph",
                    "label": f"{operation}:{symbol.symbol}",
                    "symbol": symbol.symbol,
                    "arguments": {
                        "op": operation,
                        "path": symbol.path,
                        "symbol": symbol.symbol,
                        "depth": 3 if operation != "callers" else 1,
                    },
                }
            )
    queries.extend(
        [
            {
                "tool": "search",
                "label": f"identifier:{symbols[0].symbol}",
                "symbol": symbols[0].symbol,
                "arguments": {"query": symbols[0].symbol, "topK": 20},
            },
            {
                "tool": "search",
                "label": "quoted_phrase",
                "symbol": quoted_phrase(root, symbols[0]),
                "arguments": {"query": quoted_phrase(root, symbols[0]), "topK": 20},
            },
            {
                "tool": "search",
                "label": "natural_language",
                "symbol": symbols[1].symbol,
                "arguments": {
                    "query": f"where does {symbols[1].symbol} prepare and publish project data",
                    "topK": 20,
                },
            },
        ]
    )
    for directory in outline_directories(symbols):
        queries.append(
            {
                "tool": "outline",
                "label": f"directory:{directory}",
                "symbol": directory,
                "arguments": {"target": directory, "files": True},
            }
        )
    return queries


def run_query(client: ToolClient, query: Mapping[str, Any]) -> dict[str, Any]:
    response = client.tool(str(query["tool"]), query["arguments"])
    tool = str(query["tool"])
    if tool == "callgraph":
        if response.get("code") == "callgraph_building":
            raise SoakError(f"callgraph lost readiness during {query['label']}: {response}")
        return response
    if tool == "search" and response.get("success") is True and response.get("status") != "ready":
        raise SoakError(f"search lost readiness during {query['label']}: {response}")
    return response


def compare_queries(
    root_client: ToolClient,
    baseline_client: ToolClient,
    root: Path,
    baseline: Path,
    queries: list[dict[str, Any]],
) -> tuple[str, dict[str, Any] | None]:
    prefixes = [root, baseline]
    first_divergence: dict[str, Any] | None = None
    for query in queries:
        root_response = run_query(root_client, query)
        baseline_response = run_query(baseline_client, query)
        root_output = canonical_output(root_response, prefixes)
        baseline_output = canonical_output(baseline_response, prefixes)
        difference = first_differing_line(root_output, baseline_output)
        if difference is None or first_divergence is not None:
            continue
        line, root_line, baseline_line = difference
        first_divergence = {
            "tool": query["tool"],
            "case": query["label"],
            "symbol": query["symbol"],
            "first_differing_line": line,
            "root_line": root_line,
            "baseline_line": baseline_line,
            "root_output": root_output,
            "baseline_output": baseline_output,
        }
    if first_divergence is not None:
        return (
            f"divergent({first_divergence['tool']}, {first_divergence['symbol']}, "
            f"line {first_divergence['first_differing_line']})",
            first_divergence,
        )
    return "identical", None


def regenerate_summary(result_dir: Path) -> Path:
    rows = []
    for path in sorted(result_dir.glob("probe-*.jsonl")):
        latest = latest_json_line(path)
        if latest is None:
            continue
        accounting = latest.get("accounting", {})
        divergence = latest.get("divergence")
        last_divergence = "—"
        if isinstance(divergence, dict):
            last_divergence = (
                f"{divergence.get('tool')} / {divergence.get('symbol')} / "
                f"line {divergence.get('first_differing_line')}"
            )
        rows.append(
            [
                latest.get("root", "?"),
                accounting.get("manifest_count", "?"),
                accounting.get("head_tree_count", "?"),
                "yes" if accounting.get("duplicate_generation") else "no",
                latest.get("parity", "?"),
                last_divergence,
            ]
        )
    lines = [
        "# Content-addressed views soak summary",
        "",
        "Generated by `scripts/views-soak-probe.sh`; each probe appends one immutable JSONL row.",
        "",
        "| root | generations | HEAD trees | dup generations | last parity | last divergence |",
        "|---|---:|---:|:---:|---|---|",
    ]
    for row in rows:
        lines.append("| " + " | ".join(markdown_cell(value) for value in row) + " |")
    if not rows:
        lines.append("| — | 0 | 0 | no | — | — |")
    lines.append("")
    summary = result_dir / "summary.md"
    summary.write_text("\n".join(lines), encoding="utf-8")
    return summary


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", help="One of the three configured views-soak roots")
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
    binary = args.binary.expanduser().resolve()
    storage = args.storage.expanduser().resolve()
    if not binary.is_file() or not binary.stat().st_mode & 0o111:
        raise SoakError(f"placed AFT binary is missing or not executable: {binary}")
    root, scope = resolve_root(args.root)
    project_config = read_jsonc(root / ".cortexkit" / "aft.jsonc")
    views = project_config.get("views")
    if not isinstance(views, dict) or views.get("enabled") is not True:
        raise SoakError(f"views.enabled is not true in {root}/.cortexkit/aft.jsonc")
    head = git_text(root, "rev-parse", "HEAD")

    accounting = view_accounting(storage, root, scope)
    health = health_snapshot(binary)
    baseline = ensure_owned_baseline(root, scope, head)
    user_config = Path.home() / ".config" / "cortexkit" / "aft.jsonc"
    connection_file = resolve_subc_connection_file(user_config)
    subc_probe = Path.home() / ".local/share/cortexkit/bin/subc-probe"
    if not connection_file.is_file() or not subc_probe.is_file():
        raise SoakError(
            f"daemon query prerequisites missing: connection={connection_file}, probe={subc_probe}"
        )
    cache_dir = Path.home() / ".cache" / "aft-views-soak" / scope
    baseline_storage = cache_dir / "baseline-storage"
    session = f"views-soak-probe-{int(time.time())}"
    root_client = SubcClient(subc_probe, connection_file, root, session)

    with NdjsonClient(
        binary,
        baseline,
        baseline_storage,
        cache_dir / "probe-baseline.stderr.log",
        f"views-soak-baseline-{scope}",
    ) as baseline_client:
        baseline_client.configure(user_config)
        candidates = select_stable_symbols(root_client, root, count=30)
        # A parity case must resolve in both fully-ready stores. A missing symbol
        # is retained as a comparison only after readiness, never used as the
        # readiness control itself.
        root_warmup = wait_indexes_ready(root_client, candidates[0], timeout_s=1200.0)
        baseline_warmup = wait_indexes_ready(baseline_client, candidates[0], timeout_s=1200.0)
        symbols = []
        for candidate in candidates:
            arguments = {
                "op": "callers",
                "path": candidate.path,
                "symbol": candidate.symbol,
            }
            root_check = root_client.tool("callgraph", arguments)
            baseline_check = baseline_client.tool("callgraph", arguments)
            if root_check.get("success") is True and baseline_check.get("success") is True:
                symbols.append(candidate)
            if len(symbols) == 5:
                break
        if len(symbols) != 5:
            raise SoakError(
                f"only {len(symbols)} stable symbols resolved in both fully-ready callgraphs"
            )
        queries = query_set(root, symbols)
        parity, divergence = compare_queries(
            root_client, baseline_client, root, baseline, queries
        )

    record = {
        "schema_version": 1,
        "observed_at": utc_now(),
        "root": str(root),
        "head": head,
        "baseline": str(baseline),
        "accounting": accounting,
        "health": health,
        "stable_symbols": [
            {"path": symbol.path, "symbol": symbol.symbol, "changed_at": symbol.changed_at}
            for symbol in symbols
        ],
        "query_count": len(queries),
        "readiness": {
            "views_on_daemon_ms": root_warmup["elapsed_ms"],
            "views_off_baseline_ms": baseline_warmup["elapsed_ms"],
            "baseline_storage": str(baseline_storage),
        },
        "measurement_subjects": {
            "views_on": "running_subc_daemon",
            "views_off": "standalone_owned_baseline",
        },
        "parity": parity,
        "divergence": divergence,
    }
    output = RESULT_DIR / f"probe-{scope}.jsonl"
    append_json_line(output, record)
    summary = regenerate_summary(RESULT_DIR)
    print(f"wrote {output}")
    print(f"wrote {summary}")
    print(parity)
    if divergence is not None:
        print("root output:")
        print(divergence["root_output"])
        print("baseline output:")
        print(divergence["baseline_output"])
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except (SoakError, OSError, subprocess.SubprocessError) as error:
        print(f"views-soak-probe: {error}", file=sys.stderr)
        raise SystemExit(1)
