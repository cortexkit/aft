#!/usr/bin/env python3
"""Run the production-lane Qwen3 query-instruction A/B without changing benchmark inputs."""

from __future__ import annotations

import argparse
import heapq
import json
import mmap
import re
import shutil
import struct
import tempfile
import time
import urllib.request
import zipfile
from contextlib import contextmanager
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Mapping, Optional, Sequence

import run_embed_body_cap_ab as body_cap
from metrics import evaluate_retrieval, file_path_relevance
from run import AftProtocolError, normalize_result_path
from run_exact_recall import (
    aggregate as aggregate_exact,
    evaluate_fixture as evaluate_exact_fixture,
    load_fixtures as load_exact_fixtures,
    validate_corpus,
)
from run_real_query import load_capability, load_inputs, materialized_bundle, score_manifest_rows
from search_quality_lib import aggregate_real_query
from setup_corpus import parse_corpus_toml

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
CONCEPT_TOP_K = 10
DENSE_TOP_K = 10
REAL_QUERY_PROFILE = "paged"
MODEL_CARD_TASK = "Given a web search query, retrieve relevant passages that answer the query"
CODE_SEARCH_TASK = "Given a code search query, retrieve relevant source code, symbols, and documentation"
MODEL_CARD_URL = "https://huggingface.co/Qwen/Qwen3-Embedding-0.6B#usage-tips"
CONTENT_TOKEN_RE = re.compile(r"\b[A-Za-z_$][A-Za-z0-9_$]*(?:\.[A-Za-z_$][A-Za-z0-9_$]*)*\b", re.ASCII)
CONTENT_STOP_WORDS = {
    "and", "are", "been", "but", "does", "for", "from", "had", "has", "have", "how",
    "into", "its", "not", "of", "on", "that", "the", "their", "then", "there", "these",
    "this", "those", "was", "were", "what", "when", "where", "which", "who", "why", "with",
    "would",
}

JsonObject = dict[str, Any]


@dataclass(frozen=True)
class Arm:
    name: str
    query_instruction: str
    task: Optional[str]


def arm_definitions() -> list[Arm]:
    return [
        Arm("off", "off", None),
        Arm("model-card", MODEL_CARD_TASK, MODEL_CARD_TASK),
        Arm("code-search", CODE_SEARCH_TASK, CODE_SEARCH_TASK),
    ]


class ProductionAftClient(body_cap.ProductionAftClient):
    def configure(self) -> JsonObject:
        semantic: JsonObject = {
            "backend": "openai_compatible",
            "model": self.model,
            "base_url": self.base_url,
            "timeout_ms": self.timeout_ms,
            "query_instruction": self.arm.query_instruction,
            "max_batch_size": 64,
            "max_files": 20000,
        }
        doc = {
            "search_index": True,
            "semantic_search": True,
            "callgraph_store": False,
            "semantic": semantic,
        }
        response = self.call(
            "configure",
            {
                "project_root": str(self.project_root),
                "harness": "opencode",
                "storage_dir": str(self.storage_dir),
                "config": [{
                    "tier": "user",
                    "source": "<query-instruction-ab>",
                    "doc": json.dumps(doc, separators=(",", ":")),
                }],
            },
            timeout_secs=60.0,
        )
        if response.get("success") is not True:
            raise AftProtocolError(f"configure failed: {response}")
        return response


def build_client(
    args: argparse.Namespace,
    project_root: Path,
    arm: Arm,
    corpus_name: str,
) -> tuple[Optional[tempfile.TemporaryDirectory[str]], ProductionAftClient, JsonObject, Path]:
    temporary = None
    if args.storage_root:
        storage = Path(args.storage_root).resolve() / corpus_name
        storage.mkdir(parents=True, exist_ok=True)
    else:
        temporary = tempfile.TemporaryDirectory(prefix=f"aft-query-instruction-{arm.name}-{corpus_name}-")
        storage = Path(temporary.name) / "storage"
    client = ProductionAftClient(
        Path(args.binary).resolve(), project_root, args.ready_timeout, storage,
        args.base_url, args.model, arm, args.timeout_ms,
    )
    started = time.perf_counter()
    try:
        client.configure()
        status = client.wait_for_indexes(require_search=True)
    except Exception:
        client.close()
        if temporary is not None:
            temporary.cleanup()
        raise
    index_path = body_cap.locate_semantic_bin(storage)
    metrics = body_cap.semantic_bin_metrics(index_path)
    metrics.update({
        "corpus": corpus_name,
        "project_root": str(project_root),
        "build_wall_seconds": round(time.perf_counter() - started, 3),
        "status": status.get("semantic_index"),
    })
    return temporary, client, metrics, index_path


def _read_string(data: bytes, offset: int) -> tuple[str, int]:
    size, offset = body_cap._u32(data, offset)
    raw, offset = body_cap._take(data, offset, size)
    return raw.decode("utf-8"), offset


def content_tokens(text: str) -> set[str]:
    return {
        token for raw in CONTENT_TOKEN_RE.findall(text)
        if len(token := raw.lower()) >= 3 and token not in CONTENT_STOP_WORDS
    }


def query_text(arm: Arm, query: str) -> str:
    return query if arm.task is None else f"Instruct: {arm.task}\nQuery: {query}"


def embed_query(args: argparse.Namespace, arm: Arm, query: str) -> tuple[list[float], float]:
    endpoint = args.base_url.rstrip("/") + "/embeddings"
    body = json.dumps({"model": args.model, "input": [query_text(arm, query)]}).encode()
    request = urllib.request.Request(endpoint, data=body, headers={"Content-Type": "application/json"})
    started = time.perf_counter()
    with urllib.request.urlopen(request, timeout=args.timeout_ms / 1000.0) as response:
        payload = json.load(response)
    latency_ms = (time.perf_counter() - started) * 1000.0
    return [float(value) for value in payload["data"][0]["embedding"]], latency_ms


def is_test_path(path: str) -> bool:
    lowered = path.lower().replace("\\", "/")
    parts = lowered.split("/")
    name = parts[-1]
    return (
        any(part in {"test", "tests", "__tests__", "spec", "specs"} for part in parts[:-1])
        or "_test." in name or ".test." in name or ".spec." in name
    )


def dense_probe(
    args: argparse.Namespace,
    arm: Arm,
    query: str,
    expected_files: Sequence[str],
) -> JsonObject:
    vector, latency_ms = embed_query(args, arm, query)
    return {
        "query": query,
        "expected_files": list(expected_files),
        "query_vector": vector,
        "query_embed_latency_ms": round(latency_ms, 3),
    }


def resolve_no_vocabulary_band(index_path: Path, diagnostics: Sequence[JsonObject]) -> None:
    try:
        import numpy as np
    except ImportError as error:
        raise RuntimeError(
            "the no-vocabulary pure-cosine diagnostic requires NumPy; install it in the benchmark environment"
        ) from error

    queries = np.asarray([row.pop("query_vector") for row in diagnostics], dtype=np.float64)
    if not np.isfinite(queries).all():
        raise ValueError("query embedding contains a non-finite value")
    query_norms = np.linalg.norm(queries, axis=1, keepdims=True)
    queries = np.divide(queries, query_norms, out=np.zeros_like(queries), where=query_norms != 0)
    heaps: list[list[tuple[float, int, str, str]]] = [[] for _ in diagnostics]
    serial = 0

    with index_path.open("rb") as source, mmap.mmap(source.fileno(), 0, access=mmap.ACCESS_READ) as data:
        version_raw, offset = body_cap._take(data, 0, 1)
        if version_raw[0] != 7:
            raise ValueError(f"unsupported semantic.bin version {version_raw[0]}")
        dimension, offset = body_cap._u32(data, offset)
        row_count, offset = body_cap._u32(data, offset)
        fingerprint_size, offset = body_cap._u32(data, offset)
        _, offset = body_cap._take(data, offset, fingerprint_size)
        file_count, offset = body_cap._u32(data, offset)
        for _ in range(file_count):
            offset = body_cap._skip_string(data, offset)
            _, offset = body_cap._take(data, offset, 8 + 4 + 8 + 32)

        chunk_vectors = []
        chunk_metadata: list[tuple[str, str]] = []

        def score_chunk() -> None:
            nonlocal serial
            if not chunk_vectors:
                return
            matrix = np.asarray(chunk_vectors, dtype=np.float64)
            if not np.isfinite(matrix).all():
                raise ValueError("semantic index contains a non-finite vector")
            norms = np.linalg.norm(matrix, axis=1, keepdims=True)
            matrix = np.divide(matrix, norms, out=np.zeros_like(matrix), where=norms != 0)
            if np.max(np.abs(matrix), initial=0.0) > 1.000001:
                raise ValueError("normalized semantic vector component exceeded unit range")
            if np.max(np.abs(queries), initial=0.0) > 1.000001:
                raise ValueError("normalized query vector component exceeded unit range")
            with np.errstate(over="ignore", divide="ignore", invalid="ignore"):
                scores = matrix @ queries.T
            if not np.isfinite(scores).all():
                raise ValueError("cosine score contained a non-finite value")
            for query_index, heap in enumerate(heaps):
                count = min(DENSE_TOP_K, scores.shape[0])
                candidates = np.argpartition(scores[:, query_index], -count)[-count:]
                for row_index in candidates:
                    file, embed_text = chunk_metadata[int(row_index)]
                    item = (float(scores[int(row_index), query_index]), serial, file, embed_text)
                    serial += 1
                    if len(heap) < DENSE_TOP_K:
                        heapq.heappush(heap, item)
                    elif item[0] > heap[0][0]:
                        heapq.heapreplace(heap, item)
            chunk_vectors.clear()
            chunk_metadata.clear()

        for _ in range(row_count):
            file, offset = _read_string(data, offset)
            offset = body_cap._skip_string(data, offset)  # name
            offset = body_cap._skip_string(data, offset)  # qualified name
            _, offset = body_cap._take(data, offset, 1 + 4 + 4 + 1)
            offset = body_cap._skip_string(data, offset)  # snippet
            embed_text, offset = _read_string(data, offset)
            vector_raw, offset = body_cap._take(data, offset, dimension * 4)
            if not is_test_path(file):
                chunk_vectors.append(np.frombuffer(vector_raw, dtype="<f4").copy())
                chunk_metadata.append((file, embed_text))
            if len(chunk_vectors) >= 2048:
                score_chunk()
        score_chunk()
        if offset != len(data):
            raise ValueError(f"semantic.bin trailing bytes: {len(data) - offset}")

    for diagnostic, heap in zip(diagnostics, heaps):
        query_vocabulary = content_tokens(str(diagnostic.pop("query")))
        expected = set(diagnostic.pop("expected_files"))
        ranked = sorted(heap, reverse=True)
        band = [item for item in ranked if query_vocabulary.isdisjoint(content_tokens(item[3]))]
        diagnostic.update({
            "dense_top_10_rows": len(ranked),
            "no_vocabulary_rows": len(band),
            "relevant_no_vocabulary_rows": sum(1 for item in band if item[2] in expected),
            "no_vocabulary_paths": [item[2] for item in band],
        })


def aggregate_dense(rows: Sequence[JsonObject]) -> JsonObject:
    dense_count = sum(int(row["dense_top_10_rows"]) for row in rows)
    band_count = sum(int(row["no_vocabulary_rows"]) for row in rows)
    relevant_count = sum(int(row["relevant_no_vocabulary_rows"]) for row in rows)
    latencies = [float(row["query_embed_latency_ms"]) for row in rows]
    return {
        "dense_top_10_rows": dense_count,
        "no_vocabulary_rows": band_count,
        "share_of_dense_top_10": round(band_count / dense_count, 6) if dense_count else 0.0,
        "relevant_no_vocabulary_rows": relevant_count,
        "relevance_rate_inside_band": round(relevant_count / band_count, 6) if band_count else 0.0,
        "query_embed_latency_ms_p50": body_cap.percentile(latencies, 50),
        "query_embed_latency_ms_p95": body_cap.percentile(latencies, 95),
    }


def concept_family(args: argparse.Namespace, arm: Arm) -> tuple[JsonObject, JsonObject]:
    fixture_path = Path(args.concept_fixtures).resolve()
    fixtures = json.loads(fixture_path.read_text())
    rows: list[JsonObject] = []
    with body_cap.materialized_git_head(ROOT) as project_root:
        temporary, client, index_metrics, index_path = build_client(args, project_root, arm, "concept-aft")
        try:
            for index, fixture in enumerate(fixtures):
                response, latency_ms = client.semantic_search(str(fixture["query"]), CONCEPT_TOP_K)
                if response.get("success") is False or response.get("status") != "ready":
                    raise AftProtocolError(f"concept query failed: {response}")
                results = response.get("results") or []
                predictions = [
                    {"file": normalize_result_path(str(result.get("file", "")), project_root)}
                    for result in results[:CONCEPT_TOP_K] if isinstance(result, Mapping)
                ]
                expected = list(fixture["expected_top_files"])
                metrics = evaluate_retrieval(predictions, [{"file": path} for path in expected], file_path_relevance)
                expected_set = set(expected)
                rank = next((rank for rank, result in enumerate(predictions, 1) if result["file"] in expected_set), None)
                dense = dense_probe(args, arm, str(fixture["query"]), expected)
                rows.append({
                    "query_key": f"concept:{index}", "query": fixture["query"], "shape": fixture["shape"],
                    "expected": expected, "top_k": CONCEPT_TOP_K, "rank": rank, "metrics": metrics,
                    "latency_ms": round(latency_ms, 3), "result_files": [result["file"] for result in predictions],
                    "dense_diagnostic": dense,
                })
            resolve_no_vocabulary_band(index_path, [row["dense_diagnostic"] for row in rows])
        finally:
            client.close()
            if temporary is not None:
                temporary.cleanup()
    family = {
        "fixture_sha256": body_cap.sha256_file(fixture_path), "query_count": len(rows), "top_k": CONCEPT_TOP_K,
        "mrr_at_10": round(sum(float(row["metrics"]["mrr"]) for row in rows) / len(rows), 6),
        "hit_at_1": round(sum(1 for row in rows if row["rank"] == 1) / len(rows), 6),
        "no_vocabulary_band": aggregate_dense([row["dense_diagnostic"] for row in rows]), "rows": rows,
    }
    return family, index_metrics


def exact_family(args: argparse.Namespace, arm: Arm) -> tuple[JsonObject, list[JsonObject]]:
    corpus_path = Path(args.corpus).resolve()
    corpus, repos = parse_corpus_toml(corpus_path)
    clone_root = validate_corpus(corpus_path, corpus, repos)
    fixture_path = Path(args.exact_fixtures).resolve()
    _, fixtures = load_exact_fixtures(fixture_path, [str(repo["name"]) for repo in repos])
    rows: list[JsonObject] = []
    indexes: list[JsonObject] = []
    for repo in repos:
        repo_name = str(repo["name"])
        repo_root = clone_root / repo_name
        temporary, client, index_metrics, index_path = build_client(args, repo_root, arm, f"exact-{repo_name}")
        indexes.append(index_metrics)
        try:
            for fixture in fixtures:
                if fixture["repo"] != repo_name:
                    continue
                row = evaluate_exact_fixture(client, fixture, repo_root)
                rank = row["rank"]
                row.update({
                    "query_key": f"exact:{row['id']}", "query": fixture["query"],
                    "expected": row["expected_file"], "top_k": 5 if fixture["family"] == "sentence" else 10,
                    "mrr_at_10": round(1.0 / rank, 6) if rank else 0.0,
                    "hit_at_1": float(rank == 1),
                    "dense_diagnostic": dense_probe(
                        args, arm, fixture["query"], [row["expected_file"]]
                    ),
                })
                rows.append(row)
            resolve_no_vocabulary_band(
                index_path,
                [row["dense_diagnostic"] for row in rows if row["repo"] == repo_name],
            )
        finally:
            client.close()
            if temporary is not None:
                temporary.cleanup()
    exact_metrics = aggregate_exact(rows)
    family = {
        "fixture_sha256": body_cap.sha256_file(fixture_path), "corpus_sha256": body_cap.sha256_file(corpus_path),
        "query_count": len(rows), "top_k_by_family": {"sentence": 5, "pair": 10},
        "mrr_at_10": round(sum(float(row["mrr_at_10"]) for row in rows) / len(rows), 6),
        "hit_at_1": round(sum(float(row["hit_at_1"]) for row in rows) / len(rows), 6),
        "sentence_rank1": exact_metrics["sentence_rank1"], "pair_recall_at_10": exact_metrics["pair_recall_at_10"],
        "no_vocabulary_band": aggregate_dense([row["dense_diagnostic"] for row in rows]), "rows": rows,
    }
    return family, indexes


@contextmanager
def materialized_real_bundle(bundle: Path, storage_root: Optional[str]):
    if not storage_root:
        with materialized_bundle(bundle) as root:
            yield root
        return
    corpus_dir = Path(storage_root).resolve() / "corpora"
    root = corpus_dir / "real-query-tree"
    marker = corpus_dir / "real-query-tree.sha256"
    bundle_hash = body_cap.sha256_file(bundle)
    if not marker.is_file() or marker.read_text().strip() != bundle_hash:
        if root.exists():
            shutil.rmtree(root)
        root.mkdir(parents=True)
        with zipfile.ZipFile(bundle) as archive:
            for info in archive.infolist():
                target = (root / info.filename).resolve()
                mode = (info.external_attr >> 16) & 0o170000
                if root.resolve() not in target.parents or info.is_dir() or mode == 0o120000:
                    raise ValueError(f"invalid real-query bundle member: {info.filename}")
            archive.extractall(root)
        marker.write_text(bundle_hash + "\n")
    yield root


def real_query_family(args: argparse.Namespace, arm: Arm) -> tuple[JsonObject, JsonObject]:
    manifest_path = Path(args.real_manifest).resolve()
    manifest, bundle, _, _ = load_inputs(manifest_path)
    capability = load_capability(Path(args.schema).resolve())
    with materialized_real_bundle(bundle, args.storage_root) as project_root:
        temporary, client, index_metrics, index_path = build_client(args, project_root, arm, "real-query-b0")
        try:
            rows = score_manifest_rows(manifest, REAL_QUERY_PROFILE, capability, client, project_root)
            manifest_rows = {str(row["episode_id"]): row for row in manifest["rows"]}
            for row in rows:
                source = manifest_rows[str(row["episode_id"])]
                opened_file = str(source["opened_file"])
                try:
                    rank: Optional[int] = row["ranked_paths"].index(opened_file) + 1
                except ValueError:
                    rank = None
                row.update({
                    "query_key": f"real:{row['episode_id']}", "query": source["query"], "expected": opened_file,
                    "top_k": 100, "rank": rank,
                    "dense_diagnostic": dense_probe(
                        args, arm, source["query"], [opened_file]
                    ),
                })
            resolve_no_vocabulary_band(index_path, [row["dense_diagnostic"] for row in rows])
        finally:
            client.close()
            if temporary is not None:
                temporary.cleanup()
    aggregate = aggregate_real_query(rows)["family"]
    family = {
        "manifest_sha256": body_cap.sha256_file(manifest_path), "query_count": len(rows),
        "profile": REAL_QUERY_PROFILE, "top_k": 100,
        "mrr_at_10": round(float(aggregate["mrr_at_10"]), 6), "hit_at_1": round(float(aggregate["hit_at_1"]), 6),
        "hit_at_5": round(float(aggregate["hit_at_5"]), 6),
        "no_vocabulary_band": aggregate_dense([row["dense_diagnostic"] for row in rows]), "rows": rows,
    }
    return family, index_metrics


def rank_rows(arm: Mapping[str, Any]) -> dict[str, JsonObject]:
    result: dict[str, JsonObject] = {}
    for family_name, family in arm["families"].items():
        for row in family["rows"]:
            result[str(row["query_key"])] = {
                "family": family_name, "query": row["query"], "expected": row["expected"],
                "top_k": row.get("top_k", 10), "rank": row["rank"],
                "result_files": row.get("result_files", row.get("ranked_paths", [])),
            }
    return result


def movers(control: Mapping[str, Any], treatment: Mapping[str, Any]) -> JsonObject:
    before_rows, after_rows = rank_rows(control), rank_rows(treatment)
    if before_rows.keys() != after_rows.keys():
        raise ValueError("A/B query populations differ")
    changes = []
    for key, before in before_rows.items():
        after = after_rows[key]
        miss_rank = min(int(before["top_k"]), 10) + 1
        old_rank = int(before["rank"]) if before["rank"] is not None else miss_rank
        new_rank = int(after["rank"]) if after["rank"] is not None else miss_rank
        changes.append({
            "query_key": key, "family": before["family"], "query": before["query"], "expected": before["expected"],
            "before_rank": before["rank"], "after_rank": after["rank"], "rank_delta": old_rank - new_rank,
            "before_top": before["result_files"], "after_top": after["result_files"],
        })
    improved = sorted((row for row in changes if row["rank_delta"] > 0), key=lambda row: (-row["rank_delta"], row["query_key"]))
    worsened = sorted((row for row in changes if row["rank_delta"] < 0), key=lambda row: (row["rank_delta"], row["query_key"]))
    return {"improved": improved, "worsened": worsened}


def arm_totals(arm: Arm, indexes: Sequence[JsonObject], families: Mapping[str, JsonObject]) -> JsonObject:
    diagnostics = [row["dense_diagnostic"] for family in families.values() for row in family["rows"]]
    return {
        "arm": asdict(arm),
        "index": {
            "corpora": len(indexes), "build_wall_seconds": round(sum(float(item["build_wall_seconds"]) for item in indexes), 3),
            "rows": sum(int(item["rows"]) for item in indexes), "semantic_bin_bytes": sum(int(item["bytes"]) for item in indexes),
            "per_corpus": list(indexes),
        },
        "query_embed_latency_ms": {
            "scope": "direct Bionic /embeddings calls for every committed benchmark query",
            "p50": aggregate_dense(diagnostics)["query_embed_latency_ms_p50"],
            "p95": aggregate_dense(diagnostics)["query_embed_latency_ms_p95"],
        },
        "no_vocabulary_band": aggregate_dense(diagnostics),
        "families": dict(families),
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default=str(ROOT / "target/release/aft"))
    parser.add_argument("--base-url", default="http://localhost:1234/v1")
    parser.add_argument("--model", default="text-embedding-qwen3-embedding-0.6b")
    parser.add_argument("--timeout-ms", type=int, default=60000)
    parser.add_argument("--ready-timeout", type=float, default=1200.0)
    parser.add_argument("--concept-fixtures", default=str(HERE / "fixtures.json"))
    parser.add_argument("--exact-fixtures", default=str(HERE / "exact-recall-fixtures.json"))
    parser.add_argument("--corpus", default=str(HERE / "corpus/corpus.toml"))
    parser.add_argument("--real-manifest", default=str(HERE / "real-query-manifest.json"))
    parser.add_argument("--schema", default=str(ROOT / "packages/pi-plugin/src/tools/semantic.ts"))
    parser.add_argument("--output", default=str(HERE / ".bench/query-instruction-ab/results.json"))
    parser.add_argument("--storage-root", help="reuse semantic indexes across arms; query instructions never change their fingerprint")
    parser.add_argument("--arms", nargs="+", choices=[arm.name for arm in arm_definitions()])
    parser.add_argument("--families", nargs="+", choices=["concept", "exact-recall", "real-query"])
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    binary = Path(args.binary).resolve()
    if not binary.is_file():
        raise FileNotFoundError(f"aft binary not found: {binary}")
    output_path = Path(args.output).resolve()
    output_path.parent.mkdir(parents=True, exist_ok=True)
    selected = set(args.arms or [arm.name for arm in arm_definitions()])
    selected_families = set(args.families or ["concept", "exact-recall", "real-query"])
    report: JsonObject = {
        "schema": "aft-query-instruction-ab-v1", "generated_at": datetime.now(timezone.utc).isoformat(),
        "binary": {"path": str(binary), "sha256": body_cap.sha256_file(binary)},
        "production_lane": {"base_url": args.base_url, "model": args.model},
        "model_card": {"url": MODEL_CARD_URL, "verbatim_retrieval_task": MODEL_CARD_TASK}, "arms": [],
    }
    for arm in arm_definitions():
        if arm.name not in selected:
            continue
        print(f"arm_start:{arm.name}", flush=True)
        families: dict[str, JsonObject] = {}
        indexes: list[JsonObject] = []
        if "concept" in selected_families:
            concept, concept_index = concept_family(args, arm)
            families["concept"] = concept
            indexes.append(concept_index)
        if "exact-recall" in selected_families:
            exact, exact_indexes = exact_family(args, arm)
            families["exact_recall"] = exact
            indexes.extend(exact_indexes)
        if "real-query" in selected_families:
            real_query, real_index = real_query_family(args, arm)
            families["real_query"] = real_query
            indexes.append(real_index)
        report["arms"].append(arm_totals(arm, indexes, families))
        output_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        print(f"arm_done:{arm.name}", flush=True)
    by_name = {arm["arm"]["name"]: arm for arm in report["arms"]}
    for before, after in (("off", "model-card"), ("off", "code-search"), ("model-card", "code-search")):
        if before in by_name and after in by_name:
            report[f"movers_{before}_to_{after}"] = movers(by_name[before], by_name[after])
    output_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(f"report:{output_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
