#!/usr/bin/env python3
"""Recall audit: for each known answer, find the stage where aft_search loses it.

Report-only. The runner replays three sets of queries with the aft process's
benchmark-only recall audit switched on (`AFT_SEARCH_RECALL_AUDIT=1`, see
crates/aft/src/commands/semantic_search/recall_audit.rs), and classifies each
answer by the last stage it reached:

- never_indexed: neither the trigram index nor the semantic store holds the file.
- not_produced: the file is indexed, but no lane that ran scores it at any depth.
- not_admitted: a lane that ran scores it, but only beyond a candidate limit
  (the lexical lane's rarest-trigram discovery pool, the semantic lane's
  enumeration limit, or the ranked list's block depth).
- ranked_below_page: it is in the ranked list, below the returned page.
- dropped_by_dedupe: its file is on the page, but through another chunk; the
  semantic lane did produce the answer's own chunk and the one-row-per-file
  collapse discarded it. Line-ranged answers only.
- file_shown_other_span: its file is on the page through another span, and no
  lane produced the answer's own span. Line-ranged answers only.
- found: on the page (at the answer's lines, for line-ranged answers).

Sources:

- real-query: the 43 included rows of real-query-manifest.json, on the
  gate's pinned evidence tree. With the default `pack` backend the replay uses
  the gate's own vector pack and fixture embedding server, so ranks match the
  gate; those vectors are hash-derived stand-ins, not semantic, so semantic
  ranks in that mode say nothing about semantic relevance. `--real-query-backend
  local` (or `both`) replays the same rows with the live local model.
- prefrontal: prefrontal-search-fixtures.json on the prefrontal pin.
- named: named-case-fixtures.json, report-only rows for failure modes the gate
  has no row for, including validated no-answer queries.

Every AFT process is a standalone binary with a temporary storage directory; the
runner never talks to a shared daemon. The live-model runs use the managed ONNX
Runtime and model cache, like run_prefrontal_search.py.

    python3 benchmarks/aft-search/provision_corpus.py
    python3 benchmarks/aft-search/provision_evidence.py
    python3 benchmarks/aft-search/provision_corpus.py --corpus benchmarks/aft-search/corpus/prefrontal.toml
    cd benchmarks/aft-search
    python3 run_recall_audit.py --out results/recall-audit-<date>.json --markdown results/recall-audit-<date>.md
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import tempfile
from contextlib import ExitStack
from pathlib import Path
from typing import Any, Dict, Iterable, List, Mapping, Optional, Sequence, Tuple

from metrics import line_overlap_relevance
from run import AftClient, AftProtocolError, binary_sha256, binary_version, normalize_result_path
from run_exact_recall import CorpusMissing, validate_corpus
from run_prefrontal_search import check_ground_truth, ensure_local_model_env
from search_quality_lib import PAGE_SIZE
from setup_corpus import parse_corpus_toml
import run_real_query as rq

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
JsonObject = Dict[str, Any]

AUDIT_ENV = "AFT_SEARCH_RECALL_AUDIT"
TARGETS_ENV = "AFT_SEARCH_RECALL_AUDIT_TARGETS"
SOURCES = ("real-query", "prefrontal", "named")
# How far an answer got, used to pick a row's best answer when it has several.
STAGE_ORDER = {
    "never_indexed": 0,
    "not_produced": 1,
    "not_admitted": 2,
    "ranked_below_page": 3,
    "file_shown_other_span": 4,
    "dropped_by_dedupe": 4,
    "found": 5,
}
TOP_FIVE = 5


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", default=os.environ.get("AFT_BINARY_PATH", str(ROOT / "target/release/aft")))
    parser.add_argument("--sources", default=",".join(SOURCES), help="Comma-separated subset of: " + ", ".join(SOURCES))
    parser.add_argument("--real-query-backend", choices=("pack", "local", "both"), default="pack")
    parser.add_argument("--manifest", default=str(HERE / "real-query-manifest.json"))
    parser.add_argument("--prefrontal-corpus", default=str(HERE / "corpus/prefrontal.toml"))
    parser.add_argument("--prefrontal-fixtures", default=str(HERE / "prefrontal-search-fixtures.json"))
    parser.add_argument("--named-fixtures", default=str(HERE / "named-case-fixtures.json"))
    parser.add_argument("--top-k", type=int, default=PAGE_SIZE, help="Results requested per query (one page, no offset).")
    parser.add_argument("--ready-timeout", type=float, default=3600.0)
    parser.add_argument("--out", default=str(HERE / ".bench/recall-audit/audit.json"))
    parser.add_argument("--markdown", default=None, help="Optional path for the Markdown tables.")
    return parser.parse_args(list(argv))


# ---------------------------------------------------------------- case loading


def real_query_cases(manifest: Mapping[str, Any]) -> List[JsonObject]:
    cases = []
    for row in manifest.get("rows", []):
        if "excluded_reason" in row:
            continue
        cases.append(
            {
                "id": row["episode_id"],
                "source": "real-query",
                "case_class": row["mechanism"],
                "repo": "aft-evidence",
                "query": row["query"],
                "include_tests": bool(row["include_tests"]),
                "expect_no_answer": False,
                "line_level": False,
                "truths": [{"file_path": row["opened_file"], "relevance": 1, "symbol": "opened file"}],
            }
        )
    return cases


def fixture_cases(path: Path, source: str, class_key: str) -> List[JsonObject]:
    payload = json.loads(path.read_text())
    cases = []
    for task in payload["tasks"]:
        no_answer = bool(task.get("expect_no_answer", False))
        if not no_answer and not task.get("ground_truth"):
            raise ValueError(f"{source} task {task['id']} has no ground truth and does not expect no answer")
        if no_answer and task.get("ground_truth"):
            raise ValueError(f"{source} task {task['id']} expects no answer but lists ground truth")
        cases.append(
            {
                "id": task["id"],
                "source": source,
                "case_class": task[class_key],
                "repo": task["repo"],
                "query": task["query"],
                "include_tests": bool(task["include_tests"]),
                "expect_no_answer": no_answer,
                "line_level": True,
                "truths": list(task.get("ground_truth", [])),
                "verification": task.get("verification"),
            }
        )
    return cases


# ------------------------------------------------------------------ analysis


def _rank_of(paths: Iterable[str], target: str) -> Optional[int]:
    for index, path in enumerate(paths, start=1):
        if path == target:
            return index
    return None


def _chunk_overlaps(chunk: Mapping[str, Any], truth: Mapping[str, Any]) -> bool:
    prediction = {"file_path": chunk["file"], "line_start": chunk["start_line"], "line_end": chunk["end_line"]}
    return line_overlap_relevance(prediction, truth)


def _best_chunk_rank(chunks: Optional[Sequence[Mapping[str, Any]]], truth: Mapping[str, Any], line_level: bool) -> Optional[int]:
    if chunks is None:
        return None
    for chunk in chunks:
        if chunk["file"] != truth["file_path"]:
            continue
        if line_level and not _chunk_overlaps(chunk, truth):
            continue
        return int(chunk["rank"])
    return None


def _normalized_audit(audit: Mapping[str, Any], root: Path) -> JsonObject:
    """Paths in the audit are project-relative already; normalize the rare absolute one."""
    norm = lambda raw: normalize_result_path(str(raw), root)  # noqa: E731
    lanes = audit.get("lanes", {})
    chunks = lanes.get("semantic_chunks")
    return {
        "shape": audit.get("shape"),
        "lanes_run": list(audit.get("lanes_run", [])),
        "semantic_ran": bool(audit.get("semantic_ran")),
        "exact": [norm(item["file"]) for item in lanes.get("exact", [])],
        "lexical": [norm(path) for path in lanes.get("lexical", [])],
        "lexical_pool_size": lanes.get("lexical_pool_size"),
        "semantic_chunks": None if chunks is None else [dict(chunk, file=norm(chunk["file"])) for chunk in chunks],
        "path_lookup": [norm(path) for path in lanes.get("path_lookup", [])],
        "canonical": [norm(entry["file"]) for entry in audit.get("canonical_list", [])],
        "targets": audit.get("targets", {}),
        "semantic_coverage_error": audit.get("semantic_coverage_error"),
        "targets_error": audit.get("targets_error"),
        "retrieval_depth": audit.get("retrieval_depth"),
        "limits": audit.get("limits"),
    }


def classify_truth(
    truth: Mapping[str, Any],
    line_level: bool,
    predictions: Sequence[Mapping[str, Any]],
    audit: Mapping[str, Any],
    root: Path,
) -> JsonObject:
    file_path = str(truth["file_path"])
    coverage = audit["targets"].get(file_path, {})
    store_chunks = coverage.get("semantic_chunks")
    if store_chunks is not None:
        store_chunks = [dict(chunk, file=normalize_result_path(str(chunk["file"]), root)) for chunk in store_chunks]
    lanes_run = set(audit["lanes_run"])
    semantic_ran = audit["semantic_ran"]

    page_file = next((int(p["rank"]) for p in predictions if p["file_path"] == file_path), None)
    page_line = (
        next((int(p["rank"]) for p in predictions if line_overlap_relevance(p, truth)), None) if line_level else page_file
    )
    exact_rank = _rank_of(audit["exact"], file_path)
    lexical_rank = _rank_of(audit["lexical"], file_path)
    path_rank = _rank_of(audit["path_lookup"], file_path)
    canonical_rank = _rank_of(audit["canonical"], file_path)
    lane_chunk_file = _best_chunk_rank(audit["semantic_chunks"], truth, False)
    lane_chunk_line = _best_chunk_rank(audit["semantic_chunks"], truth, True) if line_level else lane_chunk_file
    store_chunk_file = _best_chunk_rank(store_chunks, truth, False)
    store_chunk_line = _best_chunk_rank(store_chunks, truth, True) if line_level else store_chunk_file
    lexical_unpooled = coverage.get("lexical_unpooled_rank")
    trigram_state = coverage.get("trigram_index")

    if page_line is not None:
        stage, detail = "found", f"page rank {page_line}"
    elif page_file is not None:
        if lane_chunk_line is not None:
            stage, detail = "dropped_by_dedupe", f"file at page rank {page_file}; answer chunk was semantic lane chunk {lane_chunk_line}"
        else:
            stage, detail = "file_shown_other_span", f"file at page rank {page_file} through another span"
    elif canonical_rank is not None:
        stage, detail = "ranked_below_page", f"ranked list position {canonical_rank}"
    elif exact_rank or lexical_rank or path_rank or lane_chunk_file:
        produced = [
            name
            for name, rank in (("exact", exact_rank), ("lexical", lexical_rank), ("path_lookup", path_rank), ("semantic", lane_chunk_file))
            if rank
        ]
        stage, detail = "not_admitted", f"produced by {'+'.join(produced)} but past the ranked list's block depth {audit['retrieval_depth']}"
    elif ("lexical" in lanes_run and lexical_unpooled) or (semantic_ran and store_chunk_file):
        cut = []
        if "lexical" in lanes_run and lexical_unpooled:
            cut.append(f"lexical discovery pool (would rank {lexical_unpooled} unpooled)")
        if semantic_ran and store_chunk_file:
            cut.append(f"semantic enumeration limit (best chunk {store_chunk_file} of the whole store)")
        stage, detail = "not_admitted", "cut by " + " and ".join(cut)
    elif trigram_state == "indexed" or (store_chunks or []):
        stage, detail = "not_produced", "indexed, but no lane that ran scores it"
    else:
        stage = "never_indexed"
        detail = f"trigram index: {trigram_state}; semantic chunks: {'unknown' if store_chunks is None else len(store_chunks)}"

    return {
        "file_path": file_path,
        "line_start": truth.get("line_start"),
        "line_end": truth.get("line_end"),
        "relevance": truth.get("relevance", 1),
        "symbol": truth.get("symbol"),
        "stage": stage,
        "detail": detail,
        "ranks": {
            "page_file": page_file,
            "page_line": page_line if line_level else None,
            "ranked_list": canonical_rank,
            "exact": exact_rank,
            "lexical": lexical_rank,
            "lexical_unpooled": lexical_unpooled,
            "path_lookup": path_rank,
            "semantic_lane_chunk": lane_chunk_line,
            "semantic_lane_file": lane_chunk_file,
            "semantic_store_chunk": store_chunk_line,
            "semantic_store_file": store_chunk_file,
        },
        "trigram_index": trigram_state,
        "semantic_store_chunks": None if store_chunks is None else len(store_chunks),
    }


def best_truth(truths: Sequence[JsonObject]) -> JsonObject:
    def key(item: JsonObject) -> Tuple[int, int, int]:
        rank = item["ranks"]["page_line"] or item["ranks"]["page_file"] or item["ranks"]["ranked_list"] or 10**9
        return (STAGE_ORDER[item["stage"]], int(item["relevance"]), -int(rank))

    return max(truths, key=key)


def predictions_from(response: Mapping[str, Any], root: Path) -> List[JsonObject]:
    predictions = []
    for rank, result in enumerate(response.get("results", []), start=1):
        if not isinstance(result, dict):
            continue
        predictions.append(
            {
                "rank": rank,
                "file_path": normalize_result_path(str(result.get("file", "")), root),
                "line_start": result.get("start_line"),
                "line_end": result.get("end_line"),
                "exact": result.get("exact"),
                "source": result.get("source"),
            }
        )
    return predictions


def analyse_case(case: Mapping[str, Any], response: Mapping[str, Any], root: Path) -> JsonObject:
    plan = (response.get("structuredContent") or {}).get("plan") or {}
    predictions = predictions_from(response, root)
    raw_audit = response.get("recall_audit")
    row: JsonObject = {
        key: case[key]
        for key in ("id", "source", "case_class", "repo", "query", "include_tests", "expect_no_answer", "line_level")
    }
    row["backend"] = case["backend"]
    row["plan"] = {
        "shape": plan.get("shape"),
        "lanes_run": plan.get("lanes_run"),
        "confidence": plan.get("confidence"),
        "exact_tier": plan.get("exact_tier"),
    }
    row["query_kind"] = response.get("query_kind")
    row["top5"] = [
        {"rank": p["rank"], "file_path": p["file_path"], "line_start": p["line_start"], "exact": p["exact"]}
        for p in predictions[:TOP_FIVE]
    ]
    if case.get("verification"):
        row["verification"] = case["verification"]
    if raw_audit is None:
        # Replies that never reach the ranking engine (the regex and grep
        # routes) carry no audit. Record that instead of guessing a stage.
        row["audit_missing"] = True
        row["interpreted_as"] = response.get("interpreted_as")
    audit = _normalized_audit(raw_audit or {}, root)
    row["audit"] = {
        "shape": audit["shape"],
        "lanes_run": audit["lanes_run"],
        "semantic_ran": audit["semantic_ran"],
        "lexical_pool_size": audit["lexical_pool_size"],
        "exact_count": len(audit["exact"]),
        "ranked_list_length": len(audit["canonical"]),
        "retrieval_depth": audit["retrieval_depth"],
        "semantic_coverage_error": audit["semantic_coverage_error"],
        "targets_error": audit["targets_error"],
    }
    if case["expect_no_answer"]:
        row["stage"] = "no_answer_expected"
        row["truths"] = []
        row["answer_in_top5"] = False
        return row
    truths = [classify_truth(truth, bool(case["line_level"]), predictions, audit, root) for truth in case["truths"]]
    row["truths"] = truths
    chosen = best_truth(truths)
    row["stage"] = "audit_missing" if raw_audit is None and chosen["stage"] != "found" else chosen["stage"]
    row["best_truth"] = chosen["file_path"] + (
        f":{chosen['line_start']}-{chosen['line_end']}" if chosen.get("line_start") is not None else ""
    )
    rank_key = "page_line" if case["line_level"] else "page_file"
    row["answer_in_top5"] = any((t["ranks"][rank_key] or 10**9) <= TOP_FIVE for t in truths)
    row["file_in_top5"] = any((t["ranks"]["page_file"] or 10**9) <= TOP_FIVE for t in truths)
    return row


# ----------------------------------------------------------------- execution


def search_arguments(case: Mapping[str, Any], top_k: int) -> JsonObject:
    return {"query": case["query"], "topK": top_k, "includeTests": bool(case["include_tests"])}


def write_targets(cases: Sequence[Mapping[str, Any]], directory: Path) -> Path:
    targets = sorted({str(truth["file_path"]) for case in cases for truth in case["truths"]})
    path = directory / "recall-audit-targets.json"
    path.write_text(json.dumps(targets))
    return path


def run_local_group(binary: Path, root: Path, cases: Sequence[JsonObject], top_k: int, ready_timeout: float) -> Tuple[List[JsonObject], JsonObject]:
    storage = Path(tempfile.mkdtemp(prefix="aft-recall-audit-"))
    os.environ[TARGETS_ENV] = str(write_targets(cases, storage))
    client = AftClient(binary, root, ready_timeout, storage_dir=storage / "storage")
    rows = []
    try:
        client.configure()
        status = client.wait_for_indexes(require_search=True)
        for case in cases:
            response = client.call(
                "tool_call",
                {"session_id": "aft-search-recall-audit", "name": "search", "arguments": search_arguments(case, top_k)},
                timeout_secs=600.0,
            )
            if response.get("success") is not True:
                raise AftProtocolError(f"aft_search_failed:{case['id']}:{response}")
            rows.append(analyse_case(case, response, root))
    finally:
        client.close()
        shutil.rmtree(storage, ignore_errors=True)
    return rows, {
        "search_index": status.get("search_index"),
        "semantic_index": status.get("semantic_index"),
    }


def run_pack_group(binary: Path, root: Path, pack: Mapping[str, Any], cases: Sequence[JsonObject], top_k: int, ready_timeout: float) -> List[JsonObject]:
    rows = []
    with tempfile.TemporaryDirectory(prefix="aft-recall-audit-pack-") as run_dir:
        runtime = Path(run_dir)
        os.environ[TARGETS_ENV] = str(write_targets(cases, runtime))
        with rq.fixture_endpoint(pack, runtime / "embedding-requests.log") as endpoint:
            client = rq.NdjsonClient(binary, root, runtime / "storage", runtime / "aft.stderr")
            try:
                client.configure(endpoint, str(pack["model_id"]), ready_timeout)
                client.wait_ready(ready_timeout)
                for case in cases:
                    response = client.search(search_arguments(case, top_k))
                    rows.append(analyse_case(case, response, root))
            finally:
                client.close()
    return rows


def run(args: argparse.Namespace) -> int:
    binary = Path(args.binary).resolve()
    if not binary.is_file():
        raise FileNotFoundError(f"aft_binary_missing:{binary}")
    if not 1 <= args.top_k <= PAGE_SIZE:
        raise ValueError(f"--top-k must be within 1..{PAGE_SIZE}, the product's topK maximum")
    sources = [item.strip() for item in args.sources.split(",") if item.strip()]
    unknown = sorted(set(sources) - set(SOURCES))
    if unknown:
        raise ValueError(f"unknown sources: {unknown}")

    cases: List[JsonObject] = []
    manifest_path = Path(args.manifest).resolve()
    if "real-query" in sources:
        manifest = json.loads(manifest_path.read_text())
        backends = ("pack", "local") if args.real_query_backend == "both" else (args.real_query_backend,)
        for backend in backends:
            cases.extend(dict(case, backend=backend) for case in real_query_cases(manifest))
    if "prefrontal" in sources:
        cases.extend(dict(case, backend="local") for case in fixture_cases(Path(args.prefrontal_fixtures), "prefrontal", "failure_class"))
    if "named" in sources:
        cases.extend(dict(case, backend="local") for case in fixture_cases(Path(args.named_fixtures), "named", "case_class"))

    os.environ[AUDIT_ENV] = "1"
    model_env = ensure_local_model_env()
    repo_statuses: JsonObject = {}
    rows: List[JsonObject] = []
    with ExitStack() as stack:
        evidence_root: Optional[Path] = None
        pack: Optional[Mapping[str, Any]] = None
        if any(case["repo"] == "aft-evidence" for case in cases):
            _manifest, provisioned_tree, _pack_path, pack = rq.load_inputs(manifest_path)
            # The same runtime copy the gate indexes: the pinned tree plus the
            # answer-key ignore list, outside the parent checkout.
            evidence_root = stack.enter_context(rq.runtime_evidence_tree(provisioned_tree)).resolve()
        prefrontal_root: Optional[Path] = None
        if any(case["repo"] == "prefrontal" for case in cases):
            corpus_path = Path(args.prefrontal_corpus).resolve()
            corpus, repos = parse_corpus_toml(corpus_path)
            prefrontal_root = (validate_corpus(corpus_path, corpus, repos) / "prefrontal").resolve()
            for case in cases:
                if case["repo"] == "prefrontal" and case["truths"]:
                    check_ground_truth({"id": case["id"], "ground_truth": case["truths"]}, prefrontal_root)

        pack_cases = [case for case in cases if case["backend"] == "pack"]
        if pack_cases:
            assert evidence_root is not None and pack is not None
            rows.extend(run_pack_group(binary, evidence_root, pack, pack_cases, args.top_k, args.ready_timeout))
            repo_statuses["aft-evidence:pack"] = {"model": pack.get("model_id")}
        for repo, root in (("aft-evidence", evidence_root), ("prefrontal", prefrontal_root)):
            group = [case for case in cases if case["repo"] == repo and case["backend"] == "local"]
            if not group:
                continue
            assert root is not None
            group_rows, status = run_local_group(binary, root, group, args.top_k, args.ready_timeout)
            rows.extend(group_rows)
            repo_statuses[f"{repo}:local"] = status

    report = {
        "schema": "aft-search-recall-audit-report-v1",
        "binary": {"version": binary_version(binary), "sha256": binary_sha256(binary)},
        "top_k": args.top_k,
        "local_model_env": model_env,
        "repo_statuses": repo_statuses,
        "summary": summarize(rows),
        "rows": rows,
    }
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    if args.markdown:
        markdown = Path(args.markdown)
        markdown.parent.mkdir(parents=True, exist_ok=True)
        markdown.write_text(render_markdown(report))
    for row in rows:
        print(f"{row['source']}:{row['backend']}:{row['id']}: stage={row['stage']} confidence={row['plan']['confidence']} top5={row['answer_in_top5']}")
    print(f"wrote {out}")
    return 0


# ------------------------------------------------------------------- report


def summarize(rows: Sequence[JsonObject]) -> JsonObject:
    summary: JsonObject = {}
    for key in sorted({(row["source"], row["backend"]) for row in rows}):
        selected = [row for row in rows if (row["source"], row["backend"]) == key]
        stages: Dict[str, int] = {}
        for row in selected:
            stages[row["stage"]] = stages.get(row["stage"], 0) + 1
        calibration: Dict[str, Dict[str, int]] = {}
        for row in selected:
            label = str(row["plan"]["confidence"])
            bucket = calibration.setdefault(label, {"rows": 0, "answer_in_top5": 0})
            bucket["rows"] += 1
            bucket["answer_in_top5"] += int(bool(row["answer_in_top5"]))
        summary[f"{key[0]}:{key[1]}"] = {"rows": len(selected), "stages": stages, "confidence": calibration}
    return summary


def _fmt(rank: Optional[int]) -> str:
    return "-" if rank is None else str(rank)


def _expected(row: Mapping[str, Any]) -> str:
    if row["expect_no_answer"]:
        return "no answer"
    return row.get("best_truth", "")


def _truth_cells(row: Mapping[str, Any]) -> List[str]:
    if row["expect_no_answer"]:
        return ["-"] * 5
    truth = next(t for t in row["truths"] if row.get("best_truth", "").startswith(t["file_path"]))
    ranks = truth["ranks"]
    semantic = _fmt(ranks["semantic_store_chunk"])
    if not row["audit"]["semantic_ran"]:
        semantic = f"not run ({semantic})"
    lexical = _fmt(ranks["lexical"])
    if ranks["lexical"] is None and ranks["lexical_unpooled"] is not None:
        lexical = f"- (unpooled {ranks['lexical_unpooled']})"
    page = _fmt(ranks["page_line"]) if row["line_level"] else _fmt(ranks["page_file"])
    if row["line_level"] and ranks["page_line"] is None and ranks["page_file"] is not None:
        page = f"- (file {ranks['page_file']})"
    return [_fmt(ranks["exact"]), lexical, semantic, _fmt(ranks["ranked_list"]), page]


def render_markdown(report: Mapping[str, Any]) -> str:
    lines: List[str] = []
    for key, summary in report["summary"].items():
        stages = ", ".join(f"{stage} {count}" for stage, count in sorted(summary["stages"].items()))
        lines.append(f"- `{key}`: {summary['rows']} rows; {stages}")
    lines.append("")
    groups = sorted({(row["source"], row["backend"]) for row in report["rows"]})
    for source, backend in groups:
        lines.extend(
            [
                f"### {source} ({backend} embeddings)",
                "",
                "| Case | Class | Expected | Shape | Exact | Lexical | Semantic (store) | Ranked list | Page | Lost at | Confidence | Top-5 |",
                "| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- | --- |",
            ]
        )
        for row in report["rows"]:
            if (row["source"], row["backend"]) != (source, backend):
                continue
            cells = [
                f"`{row['id']}`",
                str(row["case_class"]),
                f"`{_expected(row)}`" if not row["expect_no_answer"] else "no answer",
                str(row["plan"]["shape"]),
                *_truth_cells(row),
                str(row["stage"]),
                str(row["plan"]["confidence"]),
                "yes" if row["answer_in_top5"] else "no",
            ]
            lines.append("| " + " | ".join(cell.replace("|", "\\|") for cell in cells) + " |")
        lines.append("")
    return "\n".join(lines)


def main(argv: Sequence[str]) -> int:
    try:
        return run(parse_args(argv))
    except (CorpusMissing, AftProtocolError, rq.AftProtocolError, rq.InputFault, TimeoutError, FileNotFoundError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
