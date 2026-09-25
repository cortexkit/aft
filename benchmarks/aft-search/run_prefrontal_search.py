#!/usr/bin/env python3
"""Replay the prefrontal search-miss rows through the public aft_search tool.

Report-only: this runner measures where each row's known answer ranks and what
ranks above it, and writes a JSON record. It has no reference to compare
against and never fails on a ranking outcome, so it is not a gate.

It differs from the neighbouring runners on purpose:

- It calls the public `search` tool through `tool_call`, the surface an agent's
  aft_search call reaches. run_external.py calls the lower-level
  `semantic_search` command, which skips the fusion and rendering that
  produced the misses these rows record.
- It uses the live local embedding model, as the agent's session did. The
  real-query gate's vector pack holds hash-derived stand-in vectors for AFT's
  own tree, which are deterministic but not semantic, and it is bound to that
  single tree.

Provision the pinned corpus first (from the repository root):

    python3 benchmarks/aft-search/provision_corpus.py --corpus benchmarks/aft-search/corpus/prefrontal.toml
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import tempfile
from pathlib import Path
from typing import Any, Dict, List, Mapping, Optional, Sequence

from metrics import evaluate_retrieval, file_path_relevance, line_overlap_relevance
from run import AftClient, AftProtocolError, binary_sha256, binary_version, git_rev, normalize_result_path
from run_exact_recall import CorpusMissing, validate_corpus
from search_quality_lib import PAGE_SIZE
from setup_corpus import parse_corpus_toml

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
JsonObject = Dict[str, Any]
# How many leading results the report names as outranking a missed answer.
OUTRANKED_LIMIT = 10
# The managed ONNX Runtime and model cache the AFT plugin installs. Pointing the
# standalone binary at them measures the same model the agent's session used
# and avoids a download inside the run.
MANAGED_ORT = Path.home() / ".local/share/cortexkit/aft/onnxruntime/1.24.4"
MANAGED_MODEL_CACHE = Path.home() / ".local/share/cortexkit/aft/semantic/models"


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", default=os.environ.get("AFT_BINARY_PATH", str(ROOT / "target/release/aft")))
    parser.add_argument("--corpus", default=str(HERE / "corpus/prefrontal.toml"))
    parser.add_argument("--fixtures", default=str(HERE / "prefrontal-search-fixtures.json"))
    parser.add_argument("--out", default=str(HERE / ".bench/prefrontal-search/score.json"))
    parser.add_argument("--top-k", type=int, default=PAGE_SIZE, help="Results requested per query (one page, no offset).")
    parser.add_argument("--ready-timeout", type=float, default=1800.0)
    return parser.parse_args(list(argv))


def display_path(path: Path) -> str:
    """Repository-relative when possible, so a committed record carries no checkout path."""
    try:
        return path.resolve().relative_to(ROOT).as_posix()
    except ValueError:
        return str(path)


def ensure_local_model_env() -> JsonObject:
    """Point AFT at the managed runtime and model cache unless the caller already did."""
    chosen: JsonObject = {}
    library = MANAGED_ORT / ("libonnxruntime.dylib" if sys.platform == "darwin" else "libonnxruntime.so")
    if not os.environ.get("ORT_DYLIB_PATH") and library.exists():
        os.environ["ORT_DYLIB_PATH"] = str(library)
    if not os.environ.get("FASTEMBED_CACHE_DIR") and MANAGED_MODEL_CACHE.is_dir():
        os.environ["FASTEMBED_CACHE_DIR"] = str(MANAGED_MODEL_CACHE)
    chosen["ort_dylib_path"] = os.environ.get("ORT_DYLIB_PATH")
    chosen["fastembed_cache_dir"] = os.environ.get("FASTEMBED_CACHE_DIR")
    return chosen


def load_tasks(path: Path, repo_names: Sequence[str]) -> JsonObject:
    payload = json.loads(path.read_text())
    tasks = payload.get("tasks") if isinstance(payload, dict) else None
    if not isinstance(tasks, list) or not tasks:
        raise ValueError("prefrontal fixtures contain no tasks")
    seen = set()
    for task in tasks:
        for key in ("id", "query", "repo", "failure_class", "include_tests", "ground_truth"):
            if key not in task:
                raise ValueError(f"task {task.get('id', '?')} missing {key}")
        if task["id"] in seen:
            raise ValueError(f"duplicate task id {task['id']}")
        seen.add(task["id"])
        if task["repo"] not in repo_names:
            raise ValueError(f"task {task['id']} names unknown repo {task['repo']}")
        if not isinstance(task["ground_truth"], list) or not task["ground_truth"]:
            raise ValueError(f"task {task['id']} has no ground_truth")
    return payload


def check_ground_truth(task: Mapping[str, Any], repo_path: Path) -> None:
    """Refuse a row whose recorded answer no longer exists in the pinned tree."""
    for truth in task["ground_truth"]:
        target = repo_path / str(truth["file_path"])
        if not target.is_file():
            raise ValueError(f"ground_truth_missing:{task['id']}:{truth['file_path']}")
        line_count = len(target.read_text(encoding="utf-8", errors="replace").splitlines())
        if int(truth["line_end"]) > line_count or int(truth["line_start"]) < 1:
            raise ValueError(f"ground_truth_out_of_range:{task['id']}:{truth['file_path']}:{line_count}")


def to_predictions(results: Sequence[Any], repo_path: Path) -> List[JsonObject]:
    predictions: List[JsonObject] = []
    for rank, result in enumerate(results, start=1):
        if not isinstance(result, dict):
            continue
        raw_path = str(result.get("file", result.get("path", result.get("file_path", ""))))
        predictions.append(
            {
                "rank": rank,
                "file_path": normalize_result_path(raw_path, repo_path),
                "line_start": result.get("start_line", result.get("line_start")),
                "line_end": result.get("end_line", result.get("line_end")),
                "name": result.get("name"),
                "kind": result.get("kind"),
                "source": result.get("source"),
                "score": result.get("score"),
            }
        )
    return predictions


def first_rank(predictions: Sequence[JsonObject], truths: Sequence[JsonObject], relevance) -> Optional[int]:
    for prediction in predictions:
        if any(relevance(prediction, truth) for truth in truths):
            return int(prediction["rank"])
    return None


def evaluate_task(client: AftClient, task: Mapping[str, Any], repo_path: Path, top_k: int) -> JsonObject:
    arguments = {"query": task["query"], "topK": top_k, "includeTests": bool(task["include_tests"])}
    response = client.call(
        "tool_call",
        {"session_id": "aft-search-prefrontal", "name": "search", "arguments": arguments},
        timeout_secs=300.0,
    )
    if response.get("success") is not True or not isinstance(response.get("results"), list):
        raise AftProtocolError(f"aft_search_failed:{task['id']}:{response}")
    predictions = to_predictions(response["results"], repo_path)
    truths = task["ground_truth"]
    for prediction in predictions:
        prediction["relevant_line_overlap"] = any(line_overlap_relevance(prediction, t) for t in truths)
        prediction["relevant_file"] = any(file_path_relevance(prediction, t) for t in truths)
    line_rank = first_rank(predictions, truths, line_overlap_relevance)
    file_rank = first_rank(predictions, truths, file_path_relevance)
    cutoff = (line_rank or (OUTRANKED_LIMIT + 1)) - 1
    per_truth = [
        {
            "file_path": truth["file_path"],
            "line_start": truth["line_start"],
            "line_end": truth["line_end"],
            "relevance": truth.get("relevance", 1),
            "symbol": truth.get("symbol"),
            "rank_line_overlap": first_rank(predictions, [truth], line_overlap_relevance),
            "rank_file": first_rank(predictions, [truth], file_path_relevance),
        }
        for truth in truths
    ]
    # Everything in the response except the result rows, so a later confidence
    # or "no strong match" signal shows up in the record without code changes.
    # The rendered text and the structured copy of the result rows repeat what
    # "results" already records, with machine-specific absolute paths.
    response_fields = {
        key: value
        for key, value in response.items()
        if key not in {"id", "results", "success", "text"}
    }
    structured = response_fields.get("structuredContent")
    if isinstance(structured, dict):
        response_fields["structuredContent"] = {
            key: value for key, value in structured.items() if key != "results"
        }
    return {
        "task_id": task["id"],
        "query": task["query"],
        "failure_class": task["failure_class"],
        "request": arguments,
        "result_count": len(predictions),
        "first_relevant_rank_line_overlap": line_rank,
        "first_relevant_rank_file": file_rank,
        "ground_truth_ranks": per_truth,
        "outranked_by": [
            {key: prediction[key] for key in ("rank", "file_path", "line_start", "line_end", "name", "kind", "source")}
            for prediction in predictions[: min(cutoff, OUTRANKED_LIMIT)]
        ],
        "retrieval_metrics_line_overlap": evaluate_retrieval(predictions, truths, line_overlap_relevance),
        "retrieval_metrics_file": evaluate_retrieval(predictions, truths, file_path_relevance),
        "response_fields": response_fields,
        "results": predictions,
    }


def run(args: argparse.Namespace) -> int:
    binary = Path(args.binary).resolve()
    if not binary.is_file():
        raise FileNotFoundError(f"aft_binary_missing:{binary}")
    if not 1 <= args.top_k <= PAGE_SIZE:
        raise ValueError(f"--top-k must be within 1..{PAGE_SIZE}, the product's topK maximum")
    corpus_path = Path(args.corpus).resolve()
    corpus, repos = parse_corpus_toml(corpus_path)
    clone_root = validate_corpus(corpus_path, corpus, repos)
    repo_names = [str(repo["name"]) for repo in repos]
    payload = load_tasks(Path(args.fixtures).resolve(), repo_names)
    model_env = ensure_local_model_env()

    evaluations: List[JsonObject] = []
    statuses: JsonObject = {}
    for repo in repos:
        name = str(repo["name"])
        repo_path = clone_root / name
        tasks = [task for task in payload["tasks"] if task["repo"] == name]
        if not tasks:
            continue
        for task in tasks:
            check_ground_truth(task, repo_path)
        storage = Path(tempfile.mkdtemp(prefix="aft-prefrontal-search-"))
        client = AftClient(binary, repo_path, args.ready_timeout, storage_dir=storage)
        try:
            client.configure()
            status = client.wait_for_indexes(require_search=True)
            statuses[name] = {
                "commit": git_rev(repo_path),
                "search_index": status.get("search_index"),
                "semantic_index": status.get("semantic_index"),
            }
            evaluations.extend(evaluate_task(client, task, repo_path, args.top_k) for task in tasks)
        finally:
            client.close()
            shutil.rmtree(storage, ignore_errors=True)

    report = {
        "schema": "aft-search-prefrontal-report-v1",
        "binary": {"path": display_path(binary), "version": binary_version(binary), "sha256": binary_sha256(binary)},
        "corpus_manifest": corpus_path.relative_to(ROOT).as_posix(),
        "fixtures": Path(args.fixtures).resolve().relative_to(ROOT).as_posix(),
        "top_k": args.top_k,
        "local_model_env": model_env,
        "repo_statuses": statuses,
        "unsupported": payload.get("unsupported", []),
        "no_answer_rows": payload.get("no_answer_rows"),
        "tasks": evaluations,
    }
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    for item in evaluations:
        print(
            f"{item['task_id']}: line_rank={item['first_relevant_rank_line_overlap']} "
            f"file_rank={item['first_relevant_rank_file']} class={item['failure_class']}"
        )
    print(f"wrote {out}")
    return 0


def main(argv: Sequence[str]) -> int:
    try:
        return run(parse_args(argv))
    except (CorpusMissing, AftProtocolError, TimeoutError, FileNotFoundError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
