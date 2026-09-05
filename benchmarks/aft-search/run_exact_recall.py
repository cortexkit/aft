#!/usr/bin/env python3
"""Gate exact lexical recall on deterministic samples from the pinned corpus."""

from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple

from run import AftClient, AftProtocolError, binary_sha256, binary_version, git_rev, normalize_result_path
from setup_corpus import parse_corpus_toml


DEFAULT_READY_TIMEOUT_SECS = 600.0
JsonObject = Dict[str, Any]


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="../../target/release/aft", help="Path to the aft binary to measure.")
    parser.add_argument("--corpus", default="corpus/corpus.toml", help="Pinned corpus manifest path.")
    parser.add_argument("--fixtures", default="exact-recall-fixtures.json", help="Exact-recall fixture JSON path.")
    parser.add_argument("--baseline", default="exact-recall-baseline.json", help="Minimum passing metrics.")
    parser.add_argument("--out", default="results/exact-recall.json", help="Detailed JSON output path.")
    parser.add_argument("--summary", default=None, help="Optional Markdown summary file to append.")
    parser.add_argument("--ready-timeout", type=float, default=DEFAULT_READY_TIMEOUT_SECS)
    return parser.parse_args(list(argv))


def resolve(script_dir: Path, value: str) -> Path:
    path = Path(value)
    return path if path.is_absolute() else (script_dir / path).resolve()


def load_fixtures(path: Path, repo_names: Sequence[str]) -> Tuple[int, List[JsonObject]]:
    payload = json.loads(path.read_text())
    if not isinstance(payload, dict) or payload.get("schema_version") != 1:
        raise ValueError("exact-recall fixtures must use schema_version 1")
    sample_count = payload.get("sample_per_family_per_repo")
    if not isinstance(sample_count, int) or sample_count <= 0:
        raise ValueError("sample_per_family_per_repo must be a positive integer")
    fixtures = payload.get("fixtures")
    if not isinstance(fixtures, list) or not fixtures:
        raise ValueError("exact-recall fixtures must contain a non-empty fixtures array")

    seen_ids = set()
    counts: Dict[Tuple[str, str], int] = {}
    known_repos = set(repo_names)
    for index, fixture in enumerate(fixtures, start=1):
        if not isinstance(fixture, dict):
            raise ValueError(f"fixture {index} must be an object")
        for key in ("id", "repo", "family", "query", "expected_file"):
            if not fixture.get(key):
                raise ValueError(f"fixture {index} missing {key}")
        fixture_id = str(fixture["id"])
        if fixture_id in seen_ids:
            raise ValueError(f"duplicate exact-recall fixture id: {fixture_id}")
        seen_ids.add(fixture_id)
        repo = str(fixture["repo"])
        family = str(fixture["family"])
        if repo not in known_repos:
            raise ValueError(f"fixture {fixture_id} names unknown repo {repo}")
        if family not in {"sentence", "pair"}:
            raise ValueError(f"fixture {fixture_id} has invalid family {family}")
        counts[(repo, family)] = counts.get((repo, family), 0) + 1

    expected_counts = {(repo, family): sample_count for repo in repo_names for family in ("sentence", "pair")}
    if counts != expected_counts:
        raise ValueError(f"exact-recall fixture family counts differ from the declared sample: {counts}")
    return sample_count, fixtures


def load_baseline(path: Path, sample_count: int) -> JsonObject:
    baseline = json.loads(path.read_text())
    if not isinstance(baseline, dict) or baseline.get("schema_version") != 1:
        raise ValueError("exact-recall baseline must use schema_version 1")
    if baseline.get("sample_per_family_per_repo") != sample_count:
        raise ValueError("exact-recall baseline sample count does not match fixtures")
    for metric in ("sentence_rank1", "pair_recall_at_10"):
        value = baseline.get(metric)
        if not isinstance(value, (int, float)) or not 0.0 <= float(value) <= 1.0:
            raise ValueError(f"exact-recall baseline has invalid {metric}")
    return baseline


def evaluate_fixture(client: AftClient, fixture: JsonObject, repo_path: Path) -> JsonObject:
    family = str(fixture["family"])
    top_k = 5 if family == "sentence" else 10
    response, latency_ms = client.semantic_search(str(fixture["query"]), top_k)
    if response.get("success") is False or response.get("status") != "ready":
        raise AftProtocolError(f"aft_search failed for {fixture['id']}: {response}")
    raw_results = response.get("results") or []
    if not isinstance(raw_results, list):
        raise AftProtocolError(f"aft_search returned non-list results for {fixture['id']}")

    expected = str(fixture["expected_file"])
    rank: Optional[int] = None
    exact_marker = False
    files: List[str] = []
    for index, result in enumerate(raw_results[:top_k], start=1):
        if not isinstance(result, dict):
            continue
        result_file = normalize_result_path(str(result.get("file", "")), repo_path)
        files.append(result_file)
        if rank is None and result_file == expected:
            rank = index
            exact_marker = result.get("exact") is True

    passed = rank == 1 if family == "sentence" else rank is not None and rank <= top_k
    return {
        "id": fixture["id"],
        "repo": fixture["repo"],
        "family": family,
        "query": fixture["query"],
        "expected_file": expected,
        "top_k": top_k,
        "rank": rank,
        "exact_marker": exact_marker,
        "passed": passed,
        "latency_ms": round(latency_ms, 3),
        "result_files": files,
    }


def metric(rows: Sequence[JsonObject], family: str) -> float:
    selected = [row for row in rows if row["family"] == family]
    return round(sum(1 for row in selected if row["passed"]) / len(selected), 6)


def aggregate(rows: Sequence[JsonObject]) -> JsonObject:
    return {
        "sentence_rank1": metric(rows, "sentence"),
        "pair_recall_at_10": metric(rows, "pair"),
        "all_passed": all(row["passed"] for row in rows),
    }


def markdown_table(rows: Sequence[JsonObject], metrics: JsonObject, baseline: JsonObject) -> str:
    lines = [
        "## AFT exact-recall gate",
        "",
        "| Repository | Family | Passed | Total | Recall | Exact markers |",
        "| --- | --- | ---: | ---: | ---: | ---: |",
    ]
    for repo in sorted({str(row["repo"]) for row in rows}):
        for family in ("sentence", "pair"):
            selected = [row for row in rows if row["repo"] == repo and row["family"] == family]
            passed = sum(1 for row in selected if row["passed"])
            exact_markers = sum(1 for row in selected if row["exact_marker"])
            lines.append(
                f"| {repo} | {family} | {passed} | {len(selected)} | "
                f"{passed / len(selected):.3f} | {exact_markers} |"
            )
    lines.extend(
        [
            "",
            f"Sentence rank-1: **{metrics['sentence_rank1']:.3f}** (baseline {float(baseline['sentence_rank1']):.3f})  ",
            f"Pair recall@10: **{metrics['pair_recall_at_10']:.3f}** (baseline {float(baseline['pair_recall_at_10']):.3f})",
            "",
        ]
    )
    return "\n".join(lines)


def main(argv: Sequence[str]) -> int:
    args = parse_args(argv)
    script_dir = Path(__file__).resolve().parent
    binary = resolve(script_dir, args.binary)
    corpus_path = resolve(script_dir, args.corpus)
    fixtures_path = resolve(script_dir, args.fixtures)
    baseline_path = resolve(script_dir, args.baseline)
    out_path = resolve(script_dir, args.out)
    if not binary.is_file():
        raise FileNotFoundError(f"aft binary not found: {binary}")

    corpus, repos = parse_corpus_toml(corpus_path)
    repo_names = [str(repo["name"]) for repo in repos]
    clone_root = Path(str(corpus.get("clone_root", ".bench/repos")))
    if not clone_root.is_absolute():
        clone_root = corpus_path.parent.parent / clone_root
    sample_count, fixtures = load_fixtures(fixtures_path, repo_names)
    baseline = load_baseline(baseline_path, sample_count)

    rows: List[JsonObject] = []
    repo_statuses: JsonObject = {}
    protocol_version: Optional[str] = None
    for repo in repos:
        repo_name = str(repo["name"])
        repo_path = clone_root / repo_name
        if not (repo_path / ".git").exists():
            raise FileNotFoundError(f"missing pinned corpus checkout: {repo_path}")
        expected_sha = str(repo["commit"])
        actual_sha = git_rev(repo_path)
        if actual_sha != expected_sha:
            raise ValueError(f"{repo_name} is at {actual_sha}, expected {expected_sha}")
        repo_fixtures = [fixture for fixture in fixtures if fixture["repo"] == repo_name]
        for fixture in repo_fixtures:
            if not (repo_path / str(fixture["expected_file"])).is_file():
                raise ValueError(f"fixture {fixture['id']} expected file is missing")

        client = AftClient(binary, repo_path, args.ready_timeout, semantic_search=False)
        try:
            client.configure()
            repo_statuses[repo_name] = client.wait_for_indexes(require_search=True)
            version = client.call("version", timeout_secs=10.0)
            if version.get("success"):
                protocol_version = protocol_version or version.get("version")
            rows.extend(evaluate_fixture(client, fixture, repo_path) for fixture in repo_fixtures)
        finally:
            client.close()

    metrics = aggregate(rows)
    report = {
        "schema_version": 1,
        "benchmark": "aft-search-exact-recall",
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "binary": {
            "path": str(binary),
            "version": protocol_version or binary_version(binary),
            "sha256": binary_sha256(binary),
        },
        "sample_per_family_per_repo": sample_count,
        "baseline": baseline,
        "metrics": metrics,
        "repo_statuses": repo_statuses,
        "results": rows,
    }
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    table = markdown_table(rows, metrics, baseline)
    print(table)
    if args.summary:
        summary_path = Path(args.summary)
        summary_path.parent.mkdir(parents=True, exist_ok=True)
        with summary_path.open("a", encoding="utf-8") as summary:
            summary.write(table)
    print(f"wrote {out_path}")

    failures = [row for row in rows if not row["passed"]]
    regressed = (
        metrics["sentence_rank1"] < float(baseline["sentence_rank1"])
        or metrics["pair_recall_at_10"] < float(baseline["pair_recall_at_10"])
    )
    if failures:
        for row in failures:
            print(f"FAIL {row['id']}: expected {row['expected_file']} rank={row['rank']} exact={row['exact_marker']}", file=sys.stderr)
    return 1 if regressed or failures else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
