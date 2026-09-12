#!/usr/bin/env python3
"""Run the production-lane embed-text body-cap A/B without changing benchmark inputs."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import struct
import subprocess
import tarfile
import tempfile
import time
import urllib.request
from contextlib import contextmanager
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterator, Mapping, Optional, Sequence

from setup_corpus import parse_corpus_toml
from metrics import evaluate_retrieval, file_path_relevance
from run import AftClient, AftProtocolError, normalize_result_path
from run_exact_recall import (
    aggregate as aggregate_exact,
    evaluate_fixture as evaluate_exact_fixture,
    load_fixtures as load_exact_fixtures,
    validate_corpus,
)
from run_real_query import (
    load_capability,
    load_inputs,
    materialized_bundle,
    score_manifest_rows,
)
from search_quality_lib import aggregate_real_query

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
SIGNATURE_CHARS = 400
MEASURED_HEADER_RESERVE_CHARS = 457
CHARS_PER_TOKEN = 3.5
CONCEPT_TOP_K = 10
REAL_QUERY_PROFILE = "paged"

JsonObject = dict[str, Any]


@dataclass(frozen=True)
class Arm:
    name: str
    requested_body_chars: int
    max_input_tokens: Optional[int]
    signature_chars: int
    body_lines: Optional[int]
    effective_body_chars: int
    total_chars: int


def tokens_for_body_chars(body_chars: int) -> int:
    return math.ceil((body_chars + SIGNATURE_CHARS + MEASURED_HEADER_RESERVE_CHARS) / CHARS_PER_TOKEN)


def arm_definitions() -> list[Arm]:
    arms = [
        Arm("body-300-control", 300, None, SIGNATURE_CHARS, 15, 300, 1600),
    ]
    for body_chars in (1000, 2500):
        tokens = tokens_for_body_chars(body_chars)
        total_chars = math.floor(tokens * CHARS_PER_TOKEN)
        arms.append(
            Arm(
                f"body-{body_chars}",
                body_chars,
                tokens,
                SIGNATURE_CHARS,
                None,
                total_chars - SIGNATURE_CHARS - MEASURED_HEADER_RESERVE_CHARS,
                total_chars,
            )
        )
    return arms


class ProductionAftClient(AftClient):
    def __init__(
        self,
        binary: Path,
        project_root: Path,
        ready_timeout_secs: float,
        storage_dir: Path,
        base_url: str,
        model: str,
        arm: Arm,
        timeout_ms: int,
    ) -> None:
        super().__init__(binary, project_root, ready_timeout_secs, storage_dir, semantic_search=True)
        self.base_url = base_url
        self.model = model
        self.arm = arm
        self.timeout_ms = timeout_ms
        self.search_latencies_ms: list[float] = []

    def configure(self) -> JsonObject:
        semantic: JsonObject = {
            "backend": "openai_compatible",
            "model": self.model,
            "base_url": self.base_url,
            "timeout_ms": self.timeout_ms,
            "max_batch_size": 64,
            "max_files": 20000,
        }
        if self.arm.max_input_tokens is not None:
            semantic["max_input_tokens"] = self.arm.max_input_tokens
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
                "config": [
                    {
                        "tier": "user",
                        "source": "<embed-body-cap-ab>",
                        "doc": json.dumps(doc, separators=(",", ":")),
                    }
                ],
            },
            timeout_secs=60.0,
        )
        if response.get("success") is not True:
            raise AftProtocolError(f"configure failed: {response}")
        return response

    def search(self, arguments: Mapping[str, Any]) -> JsonObject:
        started = time.perf_counter()
        response = self.call(
            "tool_call",
            {
                "session_id": "embed-body-cap-ab",
                "name": "search",
                "arguments": dict(arguments),
            },
            timeout_secs=60.0,
        )
        self.search_latencies_ms.append((time.perf_counter() - started) * 1000.0)
        if response.get("success") is not True or response.get("status") != "ready":
            raise AftProtocolError(f"aft_search failed: {response}")
        if not isinstance(response.get("results"), list):
            raise AftProtocolError("aft_search failed: results_not_array")
        return response


def percentile(values: Sequence[float], pct: int) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, math.ceil((pct / 100.0) * len(ordered)) - 1))
    return round(ordered[index], 3)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


@contextmanager
def materialized_git_head(root: Path) -> Iterator[Path]:
    with tempfile.TemporaryDirectory(prefix="aft-body-cap-concept-corpus-") as directory:
        destination = Path(directory) / "repo"
        destination.mkdir()
        archive_path = Path(directory) / "corpus.tar"
        with archive_path.open("wb") as archive:
            subprocess.run(
                ["git", "archive", "--format=tar", "HEAD"],
                cwd=root,
                stdout=archive,
                check=True,
            )
        with tarfile.open(archive_path) as archive:
            archive.extractall(destination)
        yield destination.resolve()


def _take(data: bytes, offset: int, size: int) -> tuple[bytes, int]:
    end = offset + size
    if end > len(data):
        raise ValueError("truncated semantic.bin")
    return data[offset:end], end


def _u32(data: bytes, offset: int) -> tuple[int, int]:
    raw, offset = _take(data, offset, 4)
    return struct.unpack("<I", raw)[0], offset


def _skip_string(data: bytes, offset: int) -> int:
    size, offset = _u32(data, offset)
    _, offset = _take(data, offset, size)
    return offset


def semantic_bin_metrics(path: Path) -> JsonObject:
    data = path.read_bytes()
    version_raw, offset = _take(data, 0, 1)
    version = version_raw[0]
    if version != 7:
        raise ValueError(f"unsupported semantic.bin version {version}")
    dimension, offset = _u32(data, offset)
    rows, offset = _u32(data, offset)
    fingerprint_size, offset = _u32(data, offset)
    fingerprint_raw, offset = _take(data, offset, fingerprint_size)
    fingerprint = json.loads(fingerprint_raw) if fingerprint_raw else None

    file_count, offset = _u32(data, offset)
    for _ in range(file_count):
        offset = _skip_string(data, offset)
        _, offset = _take(data, offset, 8 + 4 + 8 + 32)

    total_embed_chars = 0
    max_embed_chars = 0
    file_summary_rows = 0
    for _ in range(rows):
        offset = _skip_string(data, offset)  # file
        offset = _skip_string(data, offset)  # name
        offset = _skip_string(data, offset)  # qualified name
        kind_raw, offset = _take(data, offset, 1)
        if kind_raw[0] == 9:
            file_summary_rows += 1
        _, offset = _take(data, offset, 4 + 4 + 1)
        offset = _skip_string(data, offset)  # snippet
        embed_size, offset = _u32(data, offset)
        embed_raw, offset = _take(data, offset, embed_size)
        embed_chars = len(embed_raw.decode("utf-8"))
        total_embed_chars += embed_chars
        max_embed_chars = max(max_embed_chars, embed_chars)
        _, offset = _take(data, offset, dimension * 4)

    if offset != len(data):
        raise ValueError(f"semantic.bin trailing bytes: {len(data) - offset}")
    return {
        "path": str(path),
        "bytes": len(data),
        "rows": rows,
        "file_summary_rows": file_summary_rows,
        "symbol_rows": rows - file_summary_rows,
        "total_embed_chars": total_embed_chars,
        "max_embed_chars": max_embed_chars,
        "estimated_tokens": round(total_embed_chars / CHARS_PER_TOKEN),
        "fingerprint": fingerprint,
    }


def locate_semantic_bin(storage_dir: Path) -> Path:
    matches = list(storage_dir.rglob("semantic.bin"))
    if len(matches) != 1:
        raise ValueError(f"expected one semantic.bin under {storage_dir}, found {matches}")
    return matches[0]


def build_client(
    args: argparse.Namespace,
    project_root: Path,
    arm: Arm,
    corpus_name: str,
) -> tuple[tempfile.TemporaryDirectory[str], ProductionAftClient, JsonObject]:
    temporary = tempfile.TemporaryDirectory(prefix=f"aft-body-cap-{arm.name}-{corpus_name}-")
    storage = Path(temporary.name) / "storage"
    client = ProductionAftClient(
        Path(args.binary).resolve(),
        project_root,
        args.ready_timeout,
        storage,
        args.base_url,
        args.model,
        arm,
        args.timeout_ms,
    )
    started = time.perf_counter()
    try:
        client.configure()
        status = client.wait_for_indexes(require_search=True)
    except Exception:
        client.close()
        temporary.cleanup()
        raise
    semantic_ready_seconds = time.perf_counter() - started
    index_metrics = semantic_bin_metrics(locate_semantic_bin(storage))
    index_metrics.update(
        {
            "corpus": corpus_name,
            "project_root": str(project_root),
            "build_wall_seconds": round(semantic_ready_seconds, 3),
            "status": status.get("semantic_index"),
        }
    )
    return temporary, client, index_metrics


def concept_family(args: argparse.Namespace, arm: Arm) -> tuple[JsonObject, JsonObject]:
    fixture_path = Path(args.concept_fixtures).resolve()
    fixtures = json.loads(fixture_path.read_text())
    rows: list[JsonObject] = []
    with materialized_git_head(ROOT) as project_root:
        temporary, client, index_metrics = build_client(args, project_root, arm, "concept-aft")
        try:
            for index, fixture in enumerate(fixtures):
                response, latency_ms = client.semantic_search(str(fixture["query"]), CONCEPT_TOP_K)
                if response.get("success") is False or response.get("status") != "ready":
                    raise AftProtocolError(f"concept query failed: {response}")
                results = response.get("results") or []
                predictions = [
                    {"file": normalize_result_path(str(result.get("file", "")), project_root)}
                    for result in results[:CONCEPT_TOP_K]
                    if isinstance(result, Mapping)
                ]
                ground_truth = [{"file": path} for path in fixture["expected_top_files"]]
                metrics = evaluate_retrieval(predictions, ground_truth, file_path_relevance)
                expected = set(fixture["expected_top_files"])
                rank = next(
                    (rank for rank, result in enumerate(predictions, 1) if result["file"] in expected),
                    None,
                )
                rows.append(
                    {
                        "query_key": f"concept:{index}",
                        "query": fixture["query"],
                        "shape": fixture["shape"],
                        "expected": fixture["expected_top_files"],
                        "top_k": CONCEPT_TOP_K,
                        "rank": rank,
                        "metrics": metrics,
                        "latency_ms": round(latency_ms, 3),
                        "result_files": [result["file"] for result in predictions],
                    }
                )
        finally:
            client.close()
            temporary.cleanup()
    latencies = [float(row["latency_ms"]) for row in rows]
    family = {
        "fixture_sha256": sha256_file(fixture_path),
        "query_count": len(rows),
        "top_k": CONCEPT_TOP_K,
        "mrr_at_10": round(sum(float(row["metrics"]["mrr"]) for row in rows) / len(rows), 6),
        "hit_at_1": round(sum(1 for row in rows if row["rank"] == 1) / len(rows), 6),
        "latency_ms_p50": percentile(latencies, 50),
        "latency_ms_p95": percentile(latencies, 95),
        "rows": rows,
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
        temporary, client, index_metrics = build_client(args, repo_root, arm, f"exact-{repo_name}")
        indexes.append(index_metrics)
        try:
            for fixture in fixtures:
                if fixture["repo"] != repo_name:
                    continue
                row = evaluate_exact_fixture(client, fixture, repo_root)
                rank = row["rank"]
                row.update(
                    {
                        "query_key": f"exact:{row['id']}",
                        "expected": row["expected_file"],
                        "mrr_at_10": round(1.0 / rank, 6) if rank else 0.0,
                        "hit_at_1": float(rank == 1),
                    }
                )
                rows.append(row)
        finally:
            client.close()
            temporary.cleanup()

    exact_metrics = aggregate_exact(rows)
    latencies = [float(row["latency_ms"]) for row in rows]
    family = {
        "fixture_sha256": sha256_file(fixture_path),
        "corpus_sha256": sha256_file(corpus_path),
        "query_count": len(rows),
        "top_k_by_family": {"sentence": 5, "pair": 10},
        "mrr_at_10": round(sum(float(row["mrr_at_10"]) for row in rows) / len(rows), 6),
        "hit_at_1": round(sum(float(row["hit_at_1"]) for row in rows) / len(rows), 6),
        "sentence_rank1": exact_metrics["sentence_rank1"],
        "pair_recall_at_10": exact_metrics["pair_recall_at_10"],
        "latency_ms_p50": percentile(latencies, 50),
        "latency_ms_p95": percentile(latencies, 95),
        "rows": rows,
    }
    return family, indexes


def real_query_family(args: argparse.Namespace, arm: Arm) -> tuple[JsonObject, JsonObject]:
    manifest_path = Path(args.real_manifest).resolve()
    manifest, bundle, _, _ = load_inputs(manifest_path)
    capability = load_capability(Path(args.schema).resolve())
    with materialized_bundle(bundle) as project_root:
        temporary, client, index_metrics = build_client(args, project_root, arm, "real-query-b0")
        try:
            rows = score_manifest_rows(
                manifest,
                REAL_QUERY_PROFILE,
                capability,
                client,
                project_root,
            )
            latencies = list(client.search_latencies_ms)
        finally:
            client.close()
            temporary.cleanup()
    manifest_rows = {str(row["episode_id"]): row for row in manifest["rows"]}
    for row in rows:
        source = manifest_rows[str(row["episode_id"])]
        ranked_paths = row["ranked_paths"]
        opened_file = str(source["opened_file"])
        try:
            rank: Optional[int] = ranked_paths.index(opened_file) + 1
        except ValueError:
            rank = None
        row.update(
            {
                "query_key": f"real:{row['episode_id']}",
                "query": source["query"],
                "expected": opened_file,
                "top_k": 100,
                "rank": rank,
            }
        )
    aggregate = aggregate_real_query(rows)["family"]
    family = {
        "manifest_sha256": sha256_file(manifest_path),
        "query_count": len(rows),
        "profile": REAL_QUERY_PROFILE,
        "top_k": 100,
        "mrr_at_10": round(float(aggregate["mrr_at_10"]), 6),
        "hit_at_1": round(float(aggregate["hit_at_1"]), 6),
        "hit_at_5": round(float(aggregate["hit_at_5"]), 6),
        "latency_ms_p50": percentile(latencies, 50),
        "latency_ms_p95": percentile(latencies, 95),
        "request_latencies_ms": [round(value, 3) for value in latencies],
        "rows": rows,
    }
    return family, index_metrics


def arm_totals(arm: Arm, indexes: Sequence[JsonObject], families: Mapping[str, JsonObject]) -> JsonObject:
    latencies = [
        float(row["latency_ms"])
        for family_name in ("concept", "exact_recall")
        for row in families[family_name]["rows"]
    ]
    latencies.extend(float(value) for value in families["real_query"]["request_latencies_ms"])
    return {
        "arm": asdict(arm),
        "index": {
            "corpora": len(indexes),
            "build_wall_seconds": round(sum(float(item["build_wall_seconds"]) for item in indexes), 3),
            "rows": sum(int(item["rows"]) for item in indexes),
            "total_embed_chars": sum(int(item["total_embed_chars"]) for item in indexes),
            "tokens": sum(int(item["estimated_tokens"]) for item in indexes),
            "tokens_source": "estimated:embed_text_chars/3.5",
            "semantic_bin_bytes": sum(int(item["bytes"]) for item in indexes),
            "per_corpus": list(indexes),
        },
        "query_latency_ms": {
            "scope": "all requests across concept, exact-recall, and real-query families",
            "p50": percentile(latencies, 50),
            "p95": percentile(latencies, 95),
        },
        "families": dict(families),
    }


def rank_rows(arm: Mapping[str, Any]) -> dict[str, JsonObject]:
    result: dict[str, JsonObject] = {}
    for family_name, family in arm["families"].items():
        for row in family["rows"]:
            result[str(row["query_key"])] = {
                "family": family_name,
                "query": row["query"],
                "expected": row["expected"],
                "top_k": row["top_k"],
                "rank": row["rank"],
                "result_files": row.get("result_files", row.get("ranked_paths", [])),
            }
    return result


def validate_arm_contracts(arms: Sequence[Mapping[str, Any]]) -> None:
    if len(arms) < 2:
        return
    reference = rank_rows(arms[0])
    reference_contract = {
        key: (row["family"], row["query"], row["expected"], row["top_k"])
        for key, row in reference.items()
    }
    for arm in arms[1:]:
        rows = rank_rows(arm)
        contract = {
            key: (row["family"], row["query"], row["expected"], row["top_k"])
            for key, row in rows.items()
        }
        if contract != reference_contract:
            raise ValueError(f"A/B query or top-k contract differs in {arm['arm']['name']}")


def most_moved(control: Mapping[str, Any], treatment: Mapping[str, Any], limit: int = 20) -> JsonObject:
    control_rows = rank_rows(control)
    treatment_rows = rank_rows(treatment)
    if control_rows.keys() != treatment_rows.keys():
        raise ValueError("A/B query populations differ")
    changes: list[JsonObject] = []
    for key in control_rows:
        before = control_rows[key]
        after = treatment_rows[key]
        miss_rank = min(int(before["top_k"]), 10) + 1
        before_rank = int(before["rank"]) if before["rank"] is not None else miss_rank
        after_rank = int(after["rank"]) if after["rank"] is not None else miss_rank
        changes.append(
            {
                "query_key": key,
                "family": before["family"],
                "query": before["query"],
                "expected": before["expected"],
                "rank_300": before["rank"],
                "rank_1000": after["rank"],
                "rank_delta": before_rank - after_rank,
                "top_300": before["result_files"],
                "top_1000": after["result_files"],
            }
        )
    improved = sorted((row for row in changes if row["rank_delta"] > 0), key=lambda row: (-row["rank_delta"], row["query_key"]))[:limit]
    worsened = sorted((row for row in changes if row["rank_delta"] < 0), key=lambda row: (row["rank_delta"], row["query_key"]))[:limit]
    return {"improved": improved, "worsened": worsened}


def probe_usage(base_url: str, model: str, timeout_seconds: float) -> JsonObject:
    endpoint = base_url.rstrip("/") + "/embeddings"
    body = json.dumps({"model": model, "input": ["embed body cap usage probe"]}).encode()
    request = urllib.request.Request(endpoint, data=body, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=timeout_seconds) as response:
        payload = json.load(response)
    usage = payload.get("usage")
    usable = isinstance(usage, Mapping) and int(usage.get("total_tokens", 0) or 0) > 0
    return {
        "endpoint": endpoint,
        "response_has_usage": isinstance(usage, Mapping),
        "usage": usage,
        "usable_for_totals": usable,
        "token_totals_source": "server usage" if usable else "estimated:embed_text_chars/3.5",
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
    parser.add_argument("--output", default=str(HERE / ".bench/embed-body-cap-ab/results.json"))
    parser.add_argument("--arms", nargs="+", choices=[arm.name for arm in arm_definitions()])
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    binary = Path(args.binary).resolve()
    if not binary.is_file():
        raise FileNotFoundError(f"aft binary not found: {binary}")
    output_path = Path(args.output).resolve()
    output_path.parent.mkdir(parents=True, exist_ok=True)
    selected = set(args.arms or [arm.name for arm in arm_definitions()])
    report: JsonObject = {
        "schema": "aft-embed-body-cap-ab-v1",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "binary": {"path": str(binary), "sha256": sha256_file(binary)},
        "production_lane": {
            "base_url": args.base_url,
            "model": args.model,
            "usage_probe": probe_usage(args.base_url, args.model, args.timeout_ms / 1000.0),
        },
        "header_reserve": {
            "chars": MEASURED_HEADER_RESERVE_CHARS,
            "corpora": ["aft", "magic-context", "opencode", "rails", "kubernetes", "elasticsearch"],
            "maximum_source": "elasticsearch:x-pack/plugin/inference/src/yamlRestTest/resources/rest-api-spec/test/inference/47_semantic_text_knn.yml::knn query against incompatible dense_vector and semantic_text fields using query vectors returns the matching semantic vectors and failures for incompatible dims",
        },
        "arms": [],
    }

    for arm in arm_definitions():
        if arm.name not in selected:
            continue
        print(f"arm_start:{arm.name}", flush=True)
        concept, concept_index = concept_family(args, arm)
        print(f"arm_corpus_ready:{arm.name}:concept-aft", flush=True)
        exact, exact_indexes = exact_family(args, arm)
        print(f"arm_family_ready:{arm.name}:exact-recall", flush=True)
        real_query, real_index = real_query_family(args, arm)
        print(f"arm_corpus_ready:{arm.name}:real-query-b0", flush=True)
        families = {"concept": concept, "exact_recall": exact, "real_query": real_query}
        report["arms"].append(arm_totals(arm, [concept_index, *exact_indexes, real_index], families))
        output_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        print(f"arm_done:{arm.name}", flush=True)

    validate_arm_contracts(report["arms"])
    by_name = {arm["arm"]["name"]: arm for arm in report["arms"]}
    if "body-300-control" in by_name and "body-1000" in by_name:
        report["movers_300_to_1000"] = most_moved(by_name["body-300-control"], by_name["body-1000"])
    output_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(f"report:{output_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
