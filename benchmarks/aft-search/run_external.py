#!/usr/bin/env python3
"""Run AFT search against Vera's external 21-task corpus.

Task definitions and metric formulas are attributed to Vera. This runner keeps
AFT's implementation independent while producing a Vera-shaped JSON report for
Recall@1/5/10, MRR@10, nDCG@10, and latency comparisons.
"""

from __future__ import annotations

import argparse
import json
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional, Sequence, Tuple

from metrics import average_dicts, evaluate_retrieval, file_path_relevance, line_overlap_relevance
from run import AftClient, AftProtocolError, binary_sha256, binary_version, git_rev, normalize_result_path, percentile, round_float
from setup_corpus import CORPUS_MANIFEST, parse_corpus_toml


TOP_K = 10
DEFAULT_READY_TIMEOUT_SECS = 600.0
METRIC_KEYS = (
    "precision_at_1",
    "precision_at_5",
    "precision_at_10",
    "recall_at_1",
    "recall_at_5",
    "recall_at_10",
    "mrr",
    "ndcg_at_10",
)
JsonObject = Dict[str, Any]


def parse_args(argv: List[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="../../target/release/aft", help="Path to the aft binary to measure.")
    parser.add_argument("--corpus", default="corpus/corpus.toml", help="Corpus manifest path.")
    parser.add_argument("--fixtures", default="external-fixtures.json", help="Vera task fixture JSON path.")
    parser.add_argument("--results-dir", default="results", help="Directory for timestamped result JSON.")
    parser.add_argument("--out", default=None, help="Optional exact output path.")
    parser.add_argument("--ready-timeout", type=float, default=DEFAULT_READY_TIMEOUT_SECS)
    parser.add_argument("--relevance-mode", choices=("line-overlap", "file-only"), default="line-overlap")
    return parser.parse_args(argv)


def resolve_script_path(script_dir: Path, value: str) -> Path:
    path = Path(value)
    if path.is_absolute():
        return path
    return (script_dir / path).resolve()


def load_external_tasks(path: Path) -> Tuple[str, List[JsonObject]]:
    data = json.loads(path.read_text())
    if isinstance(data, list):
        tasks = data
        attribution = "Task definitions vendored from Vera eval/tasks/*.json."
    elif isinstance(data, dict):
        tasks = data.get("tasks", [])
        attribution = str(data.get("attribution", "Task definitions vendored from Vera."))
    else:
        raise ValueError("external fixtures must be a JSON object with tasks or a task array")
    if not isinstance(tasks, list) or not tasks:
        raise ValueError("external fixtures contain no tasks")
    for index, task in enumerate(tasks, start=1):
        for key in ("id", "query", "category", "repo", "ground_truth", "description"):
            if key not in task:
                raise ValueError(f"task {index} missing {key}")
        if not isinstance(task["ground_truth"], list) or not task["ground_truth"]:
            raise ValueError(f"task {task['id']} has no ground_truth")
    return attribution, tasks


def repo_paths_from_manifest(corpus_path: Path) -> Tuple[JsonObject, List[JsonObject], Dict[str, Path]]:
    corpus, repos = parse_corpus_toml(corpus_path)
    clone_root = Path(str(corpus.get("clone_root", ".bench/repos")))
    if not clone_root.is_absolute():
        clone_root = corpus_path.parent.parent / clone_root
    return corpus, repos, {str(repo["name"]): clone_root / str(repo["name"]) for repo in repos}


def response_to_predictions(results: Sequence[Any], project_root: Path) -> List[JsonObject]:
    predictions: List[JsonObject] = []
    for rank, result in enumerate(results[:TOP_K], start=1):
        if not isinstance(result, dict):
            continue
        file_path = normalize_result_path(str(result.get("file", result.get("file_path", ""))), project_root)
        prediction = {
            "rank": rank,
            "file_path": file_path,
            "line_start": result.get("start_line", result.get("line_start")),
            "line_end": result.get("end_line", result.get("line_end")),
            "score": round_float(result.get("score"), 6),
            "name": result.get("name"),
            "kind": result.get("kind"),
            "source": result.get("source"),
        }
        predictions.append(prediction)
    return predictions


def evaluate_task(client: AftClient, task: JsonObject, repo_path: Path, relevance_mode: str) -> JsonObject:
    response, latency_ms = client.semantic_search(str(task["query"]), TOP_K)
    if response.get("success") is False:
        raise AftProtocolError(f"semantic_search failed for {task['id']}: {response}")
    if response.get("status") != "ready":
        raise AftProtocolError(f"semantic_search not ready for {task['id']}: {response}")
    raw_results = response.get("results") or []
    if not isinstance(raw_results, list):
        raise AftProtocolError(f"semantic_search returned non-list results for {task['id']}: {response}")

    predictions = response_to_predictions(raw_results, repo_path)
    relevance_fn = line_overlap_relevance if relevance_mode == "line-overlap" else file_path_relevance
    retrieval_metrics = evaluate_retrieval(predictions, task["ground_truth"], relevance_fn)
    return {
        "task_id": task["id"],
        "query": task["query"],
        "category": task["category"],
        "repo": task["repo"],
        "description": task["description"],
        "ground_truth": task["ground_truth"],
        "retrieval_metrics": retrieval_metrics,
        "latency_ms": round(latency_ms, 3),
        "result_count": len(raw_results),
        "zero_results": len(raw_results) == 0,
        "results": predictions,
    }


def aggregate_evaluations(evaluations: Sequence[JsonObject]) -> JsonObject:
    metric_rows = [item["retrieval_metrics"] for item in evaluations]
    latencies = [float(item["latency_ms"]) for item in evaluations]
    return {
        "retrieval": average_dicts(metric_rows, METRIC_KEYS),
        "performance": {
            "task_count": len(evaluations),
            "zero_result_tasks": int(sum(1 for item in evaluations if item.get("zero_results"))),
            "latency_ms_p50": percentile(latencies, 50),
            "latency_ms_p95": percentile(latencies, 95),
        },
    }


def aggregate_by_category(evaluations: Sequence[JsonObject]) -> JsonObject:
    by_category: Dict[str, List[JsonObject]] = {}
    for item in evaluations:
        by_category.setdefault(str(item["category"]), []).append(item)
    return {category: aggregate_evaluations(items) for category, items in sorted(by_category.items())}


def evaluate_repo(
    binary: Path,
    repo_name: str,
    repo_path: Path,
    tasks: Sequence[JsonObject],
    ready_timeout: float,
    relevance_mode: str,
) -> Tuple[List[JsonObject], JsonObject, Optional[str]]:
    storage_dir = Path(tempfile.mkdtemp(prefix="aft-vera-suite-"))
    client = AftClient(binary, repo_path, ready_timeout, storage_dir=storage_dir)
    try:
        client.configure()
        status = client.wait_for_indexes(require_search=True)
        version_response = client.call("version", timeout_secs=10.0)
        protocol_version = version_response.get("version") if version_response.get("success") else None
        evaluations = [evaluate_task(client, task, repo_path, relevance_mode) for task in tasks]
        return evaluations, status, protocol_version
    finally:
        client.close()


def build_report(
    args: argparse.Namespace,
    attribution: str,
    tasks: Sequence[JsonObject],
    repos: Sequence[JsonObject],
    repo_paths: Dict[str, Path],
    evaluations: Sequence[JsonObject],
    repo_statuses: JsonObject,
    skipped_repos: Sequence[str],
    protocol_version: Optional[str],
) -> JsonObject:
    repo_shas = {name: git_rev(path) for name, path in sorted(repo_paths.items()) if path.exists()}
    binary = Path(args.binary).resolve()
    return {
        "tool_name": "aft_search",
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "version_info": {
            "tool_version": protocol_version or binary_version(binary),
            "binary_path": str(binary),
            "binary_sha256": binary_sha256(binary),
            "corpus_version": 1,
            "repo_shas": repo_shas,
            "config": {
                "top_k": TOP_K,
                "relevance_mode": args.relevance_mode,
                "experimental_search_index": True,
                "experimental_semantic_search": True,
                "reranker": False,
            },
        },
        "attribution": attribution,
        "task_count": len(tasks),
        "evaluated_task_count": len(evaluations),
        "skipped_repos": list(skipped_repos),
        "repo_statuses": repo_statuses,
        "per_task": list(evaluations),
        "per_category": aggregate_by_category(evaluations),
        "aggregate": aggregate_evaluations(evaluations),
    }


def print_report(report: JsonObject) -> None:
    aggregate = report["aggregate"]
    retrieval = aggregate["retrieval"]
    performance = aggregate["performance"]
    print("\nAFT Vera-compatible search benchmark")
    print(f"  tasks: {report['evaluated_task_count']}/{report['task_count']}  zero-result tasks: {performance['zero_result_tasks']}")
    print(
        "  overall: "
        f"R@1={retrieval['recall_at_1']:.3f} "
        f"R@5={retrieval['recall_at_5']:.3f} "
        f"R@10={retrieval['recall_at_10']:.3f} "
        f"MRR@10={retrieval['mrr']:.3f} "
        f"nDCG@10={retrieval['ndcg_at_10']:.3f} "
        f"p50={performance['latency_ms_p50']:.1f}ms "
        f"p95={performance['latency_ms_p95']:.1f}ms"
    )
    print("  Vera v0.7.0 (hybrid+rerank) MRR@10 = 0.91; Vera (no rerank) MRR@10 = 0.34; " f"AFT (hybrid lexical+semantic, no rerank) MRR@10 = {retrieval['mrr']:.3f}")
    print("\nPer category")
    print("  category          n  R@1    R@5    R@10   MRR@10 nDCG@10 p50 ms  p95 ms")
    for category, stats in report["per_category"].items():
        r = stats["retrieval"]
        p = stats["performance"]
        print(
            f"  {category:<16} {p['task_count']:>2}  {r['recall_at_1']:.3f}  {r['recall_at_5']:.3f}  "
            f"{r['recall_at_10']:.3f}  {r['mrr']:.3f}  {r['ndcg_at_10']:.3f}  "
            f"{p['latency_ms_p50']:>7.1f} {p['latency_ms_p95']:>7.1f}"
        )
    zero = [item["task_id"] for item in report["per_task"] if item.get("zero_results")]
    if zero:
        print(f"\nZero-result tasks: {', '.join(zero)}")
    if report.get("skipped_repos"):
        print(f"Skipped repos: {', '.join(report['skipped_repos'])}")


def main(argv: List[str]) -> int:
    args = parse_args(argv)
    script_dir = Path(__file__).resolve().parent
    args.binary = str(resolve_script_path(script_dir, args.binary))
    corpus_path = resolve_script_path(script_dir, args.corpus)
    fixtures_path = resolve_script_path(script_dir, args.fixtures)
    results_dir = resolve_script_path(script_dir, args.results_dir)
    if args.out:
        out_path = resolve_script_path(script_dir, args.out)
    else:
        stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        out_path = results_dir / f"aft-vera-suite-{stamp}.json"

    binary = Path(args.binary)
    if not binary.exists():
        raise FileNotFoundError(f"aft binary not found: {binary}")

    attribution, tasks = load_external_tasks(fixtures_path)
    _corpus, repos, repo_paths = repo_paths_from_manifest(corpus_path)
    tasks_by_repo: Dict[str, List[JsonObject]] = {}
    for task in tasks:
        tasks_by_repo.setdefault(str(task["repo"]), []).append(task)

    evaluations: List[JsonObject] = []
    repo_statuses: JsonObject = {}
    skipped_repos: List[str] = []
    protocol_version: Optional[str] = None

    for repo in repos:
        repo_name = str(repo["name"])
        repo_path = repo_paths[repo_name]
        repo_tasks = tasks_by_repo.get(repo_name, [])
        if not repo_tasks:
            continue
        if not (repo_path / ".git").exists():
            print(f"{repo_name}: missing checkout at {repo_path}; skipping {len(repo_tasks)} task(s)", file=sys.stderr)
            skipped_repos.append(repo_name)
            continue
        print(f"{repo_name}: indexing {repo_path} and evaluating {len(repo_tasks)} task(s)")
        try:
            repo_evaluations, status, version = evaluate_repo(
                binary, repo_name, repo_path, repo_tasks, args.ready_timeout, args.relevance_mode
            )
            evaluations.extend(repo_evaluations)
            repo_statuses[repo_name] = status
            protocol_version = protocol_version or version
        except Exception as error:
            print(f"{repo_name}: FAILED: {error}", file=sys.stderr)
            skipped_repos.append(repo_name)

    report = build_report(
        args,
        attribution,
        tasks,
        repos,
        repo_paths,
        evaluations,
        repo_statuses,
        skipped_repos,
        protocol_version,
    )
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print_report(report)
    print(f"\nwrote {out_path}")
    return 0 if evaluations else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
