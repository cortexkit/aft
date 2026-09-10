#!/usr/bin/env python3
"""Replay the checked-in real-query manifest through standalone AFT."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import select
import subprocess
import sys
import tempfile
import threading
import time
import zipfile
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Iterator, Mapping, Optional, Sequence

from embedding_fixture_server import Server
from search_quality_lib import (
    EVIDENCE_SHA,
    InputFault,
    aggregate_real_query,
    canonical_json,
    choose_stop,
    collapse_paths,
    mean_metrics,
    profile_requests,
    row_metrics,
    sha256_file,
    validate_profile_score,
    validate_scored_population,
)

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
DEFAULT_BINARY = os.environ.get("AFT_BINARY_PATH", str(ROOT / "target/release/aft"))
DEFAULT_SCHEMA = ROOT / "packages/pi-plugin/src/tools/semantic.ts"
PROBE_TEXT = "semantic index fingerprint probe"
JsonObject = dict[str, Any]


class AftProtocolError(RuntimeError):
    """A standalone-AFT protocol failure."""


class NdjsonClient:
    """Minimal client for configure and public tool_call requests."""

    def __init__(self, binary: Path, project_root: Path, storage_dir: Path, stderr_path: Path):
        env = os.environ.copy()
        env["AFT_STORAGE_DIR"] = str(storage_dir)
        env["FASTEMBED_CACHE_DIR"] = str(storage_dir / "model-cache")
        env["HTTP_PROXY"] = env["HTTPS_PROXY"] = env["ALL_PROXY"] = "http://127.0.0.1:9"
        env["NO_PROXY"] = "127.0.0.1,localhost,::1"
        env.setdefault("RUST_LOG", "warn")
        self.project_root = project_root
        self.storage_dir = storage_dir
        self._stderr = stderr_path.open("w+", encoding="utf-8")
        self.proc = subprocess.Popen(
            [str(binary)],
            cwd=project_root,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self._stderr,
            bufsize=0,
        )
        self._buffer = b""
        self._next_id = 0

    def close(self) -> None:
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)
        self._stderr.close()

    def call(self, command: str, params: Optional[Mapping[str, Any]] = None, timeout: float = 120.0) -> JsonObject:
        self._next_id += 1
        request_id = str(self._next_id)
        request: JsonObject = {"id": request_id, "command": command}
        if params:
            request.update(params)
        if self.proc.stdin is None or self.proc.stdout is None:
            raise AftProtocolError("aft_protocol:pipes_unavailable")
        self.proc.stdin.write(canonical_json(request))
        self.proc.stdin.flush()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise AftProtocolError(f"aft_protocol:process_exit:{self.proc.returncode}:{self.stderr_text()}")
            ready, _, _ = select.select([self.proc.stdout], [], [], min(0.1, deadline - time.monotonic()))
            if ready:
                chunk = os.read(self.proc.stdout.fileno(), 65536)
                if chunk:
                    self._buffer += chunk
            while b"\n" in self._buffer:
                line, self._buffer = self._buffer.split(b"\n", 1)
                try:
                    frame = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if str(frame.get("id")) == request_id:
                    return frame
        raise AftProtocolError(f"aft_protocol:timeout:{command}:{self.stderr_text()}")

    def stderr_text(self) -> str:
        self._stderr.flush()
        position = self._stderr.tell()
        self._stderr.seek(0)
        text = self._stderr.read()[-4000:]
        self._stderr.seek(position)
        return text.strip()

    def configure(self, endpoint: str, model_id: str, timeout: float) -> None:
        doc = {
            "search_index": True,
            "semantic_search": True,
            "callgraph_store": False,
            "semantic": {
                "backend": "openai_compatible",
                "model": model_id,
                "base_url": endpoint,
                "timeout_ms": int(timeout * 1000),
                "query_timeout_ms": int(timeout * 1000),
                "max_batch_size": 64,
                "max_files": 20000,
            },
        }
        response = self.call(
            "configure",
            {
                "project_root": str(self.project_root),
                "harness": "opencode",
                "storage_dir": str(self.storage_dir),
                "config": [{"tier": "user", "source": "<aft-search-real-query>", "doc": json.dumps(doc)}],
            },
            timeout,
        )
        if response.get("success") is not True:
            raise AftProtocolError(f"configure_failed:{response}")

    def wait_ready(self, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        last: JsonObject = {}
        while time.monotonic() < deadline:
            last = self.call("status", timeout=min(30.0, timeout))
            semantic = last.get("semantic_index", {})
            lexical = last.get("search_index", {})
            semantic_status = semantic.get("status") if isinstance(semantic, Mapping) else None
            lexical_status = lexical.get("status") if isinstance(lexical, Mapping) else None
            if semantic_status == "failed":
                raise AftProtocolError(f"semantic_index_failed:{semantic}:{self.stderr_text()}")
            if semantic_status == "ready" and lexical_status == "ready":
                return
            time.sleep(0.1)
        raise AftProtocolError(f"index_ready_timeout:{last}:{self.stderr_text()}")

    def search(self, arguments: Mapping[str, Any]) -> JsonObject:
        response = self.call(
            "tool_call",
            {"session_id": "aft-search-real-query", "name": "search", "arguments": dict(arguments)},
        )
        if response.get("success") is not True or response.get("status") != "ready":
            raise AftProtocolError(f"aft_search_failed:{response}")
        if not isinstance(response.get("results"), list):
            raise AftProtocolError("aft_search_failed:results_not_array")
        return response


def _schema_block(text: str) -> str:
    marker = "const SearchParams = Type.Object("
    start = text.find(marker)
    if start < 0:
        raise InputFault("capability_schema_invalid:SearchParams")
    end = text.find("\n);", start)
    if end < 0:
        raise InputFault("capability_schema_invalid:SearchParams")
    return text[start:end]


def load_capability(path: Path) -> JsonObject:
    raw = path.read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    try:
        value = json.loads(raw)
    except json.JSONDecodeError:
        block = _schema_block(raw.decode("utf-8"))
        offset_match = re.search(r"(?m)^\s*offset\s*:", block)
        declared = offset_match is not None
        capability: JsonObject = {
            "schema_path": _display_path(path),
            "schema_sha256": digest,
            "offset_declared": declared,
        }
        if offset_match is not None:
            tail = block[offset_match.start(): offset_match.start() + 1000]
            minimum = re.search(r"minimum\s*:\s*([0-9]+)", tail)
            maximum = re.search(r"maximum\s*:\s*([0-9]+)", tail)
            if not minimum or not maximum:
                raise InputFault("capability_schema_invalid:offset_bounds")
            capability["offset_bounds"] = {"minimum": int(minimum.group(1)), "maximum": int(maximum.group(1))}
        return capability
    if not isinstance(value, Mapping) or not isinstance(value.get("properties"), Mapping):
        raise InputFault("capability_schema_invalid")
    offset = value["properties"].get("offset")
    declared = isinstance(offset, Mapping)
    capability = {"schema_path": _display_path(path), "schema_sha256": digest, "offset_declared": declared}
    if declared:
        minimum = offset.get("minimum")
        maximum = offset.get("maximum")
        if offset.get("type") != "integer" or not isinstance(minimum, int) or not isinstance(maximum, int):
            raise InputFault("capability_schema_invalid:offset_bounds")
        capability["offset_bounds"] = {"minimum": minimum, "maximum": maximum}
    return capability


def _display_path(path: Path) -> str:
    try:
        return path.resolve().relative_to(ROOT).as_posix()
    except ValueError:
        return path.resolve().as_posix()


def _result_path(result: Any, project_root: Path) -> str:
    if not isinstance(result, Mapping):
        return ""
    raw = str(result.get("file", result.get("path", "")))
    path = Path(raw)
    if path.is_absolute():
        try:
            return path.resolve().relative_to(project_root.resolve()).as_posix()
        except ValueError:
            return path.as_posix()
    return path.as_posix()


def _normalize_results(response: Mapping[str, Any], project_root: Path) -> list[JsonObject]:
    normalized: list[JsonObject] = []
    for result in response.get("results", []):
        if isinstance(result, Mapping):
            item = dict(result)
            item["path"] = _result_path(result, project_root)
            normalized.append(item)
    return normalized


def _collapse_with_depth(results: Sequence[Mapping[str, Any]], stop: str) -> tuple[list[str], int]:
    paths: list[str] = []
    seen: set[str] = set()
    tenth_depth = 0
    for depth, result in enumerate(results, 1):
        path = str(result.get("path", ""))
        if path and path not in seen:
            seen.add(path)
            paths.append(path)
            if len(paths) == 10:
                tenth_depth = depth
                break
    return paths[:10], tenth_depth if stop == "ten_files" and tenth_depth else len(results)


def _response_exhausted(response: Mapping[str, Any], result_count: int, top_k: int) -> bool:
    return response.get("more_available") is False and result_count < top_k


def _request(arguments: Mapping[str, int], row: Mapping[str, Any]) -> JsonObject:
    request: JsonObject = {
        "query": row["query"],
        "topK": arguments["topK"],
        "includeTests": row["include_tests"],
    }
    if "offset" in arguments:
        request["offset"] = arguments["offset"]
    return request


def _run_requests(client: Any, requests: Sequence[JsonObject], project_root: Path) -> tuple[list[JsonObject], list[JsonObject]]:
    responses: list[JsonObject] = []
    results: list[JsonObject] = []
    for request in requests:
        response = client.search(request)
        responses.append(response)
        results.extend(_normalize_results(response, project_root))
    return responses, results


def _invariance(client: Any, row: Mapping[str, Any], project_root: Path) -> tuple[list[list[JsonObject]], list[str]]:
    plans = (
        [{"topK": 10, "offset": offset} for offset in range(0, 100, 10)],
        [{"topK": 25, "offset": offset} for offset in (0, 25, 50, 75)],
        [{"topK": 100, "offset": 0}],
    )
    sent: list[list[JsonObject]] = []
    collapsed: list[list[str]] = []
    for plan in plans:
        requests = [_request(item, row) for item in plan]
        _, results = _run_requests(client, requests, project_root)
        sent.append(requests)
        collapsed.append(collapse_paths(results[:100]))
    if not collapsed[0] == collapsed[1] == collapsed[2]:
        raise InputFault(f"page_invariance_failed:{row['episode_id']}")
    return sent, collapsed[0]


def score_manifest_rows(
    manifest: Mapping[str, Any],
    profile: str,
    capability: JsonObject,
    client: Any,
    project_root: Path,
) -> list[JsonObject]:
    included = [row for row in manifest.get("rows", []) if "excluded_reason" not in row]
    if not included:
        raise InputFault("empty_population")
    plans = profile_requests(profile, bool(capability.get("offset_declared")))
    scored: list[JsonObject] = []
    probe_pages: Optional[list[list[JsonObject]]] = None
    for row in included:
        requests = [_request(item, row) for item in plans]
        responses, results = _run_requests(client, requests, project_root)
        if profile == "paged" and probe_pages is None:
            probe_pages = results_by_request(responses, project_root)
        final_count = len(responses[-1].get("results", []))
        exhausted = _response_exhausted(responses[-1], final_count, int(requests[-1]["topK"]))
        page_cap = not exhausted and (
            profile == "single_page" or (profile == "paged" and len(requests) == len(plans))
        )
        ten_files = len(collapse_paths(results)) >= 10
        stop = choose_stop(page_cap=page_cap, exhausted=exhausted, ten_files=ten_files)
        ranked_paths, retrieval_depth = _collapse_with_depth(results, stop)
        page_zero_results = _normalize_results(responses[0], project_root)
        page_zero_ranked_paths = collapse_paths(
            page_zero_results, max_paths=len(page_zero_results)
        )
        invariance_sent: list[list[JsonObject]] = []
        if profile == "paged":
            invariance_sent, invariant_paths = _invariance(client, row, project_root)
            if invariant_paths != collapse_paths(results[:100]):
                raise InputFault(f"page_invariance_failed:{row['episode_id']}:scoring")
        metrics = row_metrics(ranked_paths, str(row["opened_file"]))
        scored.append(
            {
                "episode_id": row["episode_id"],
                "request": requests[0],
                "requests": requests,
                "request_count": len(requests) + sum(len(plan) for plan in invariance_sent),
                "include_tests": row["include_tests"],
                "include_tests_source": row["include_tests_source"],
                "pages_fetched": len(requests),
                "collapse_stop_reason": stop,
                "retrieval_depth": retrieval_depth,
                "ranked_paths": ranked_paths,
                **(
                    {"page_zero_ranked_paths": page_zero_ranked_paths}
                    if profile == "paged"
                    else {}
                ),
                "metrics": metrics,
                "pinned_shape": row["pinned_shape"],
                "mechanism": row["mechanism"],
                "census_stratum": row["census_stratum"],
                **({"invariance_requests": invariance_sent} if invariance_sent else {}),
            }
        )
    if profile == "paged":
        if probe_pages is None or len(probe_pages) < 2 or not probe_pages[0] or not probe_pages[1]:
            raise InputFault("capability_probe_inconsistency")
        capability["probe_pages_differ"] = probe_pages[0][0].get("path") != probe_pages[1][0].get("path")
        if capability["probe_pages_differ"] is not True:
            raise InputFault("capability_probe_inconsistency")
    return scored


def results_by_request(responses: Sequence[Mapping[str, Any]], project_root: Path) -> list[list[JsonObject]]:
    return [_normalize_results(response, project_root) for response in responses]


def _rank_metrics(rank: Any) -> JsonObject:
    rank_value = int(rank) if isinstance(rank, int) and rank > 0 else 0
    return {
        "mrr_at_10": 1.0 / rank_value if 0 < rank_value <= 10 else 0.0,
        "hit_at_1": float(rank_value == 1),
        "hit_at_5": float(0 < rank_value <= 5),
    }


def _family_from_exact(report: Mapping[str, Any]) -> tuple[JsonObject, JsonObject]:
    rows = report.get("results")
    if not isinstance(rows, list) or not rows:
        raise InputFault("malformed_schema:exact_recall_score")
    values: list[JsonObject] = []
    groups: dict[str, list[JsonObject]] = {}
    for row in rows:
        metrics = _rank_metrics(row.get("rank"))
        values.append(metrics)
        name = f"{row.get('repo')}:{row.get('family')}"
        groups.setdefault(name, []).append(metrics)
    return mean_metrics(values), {name: mean_metrics(group) for name, group in sorted(groups.items())}


def _family_from_concept(report: Mapping[str, Any]) -> tuple[JsonObject, JsonObject]:
    rows = report.get("rows")
    if not isinstance(rows, list) or not rows:
        raise InputFault("malformed_schema:concept_recall_score")
    values: list[JsonObject] = []
    groups: dict[str, list[JsonObject]] = {}
    for row in rows:
        metrics = {name: float(row[name]) for name in ("mrr_at_10", "hit_at_1", "hit_at_5")}
        values.append(metrics)
        groups.setdefault(str(row.get("fixture_group", "unknown")), []).append(metrics)
    return mean_metrics(values), {name: mean_metrics(group) for name, group in sorted(groups.items())}


def assemble_score(
    manifest: Mapping[str, Any],
    rows: Sequence[Mapping[str, Any]],
    profile: str,
    capability: Mapping[str, Any],
    model_id: str,
    exact_report: Mapping[str, Any],
    concept_report: Mapping[str, Any],
    manifest_path: Path,
    binary: Path,
    reference: Path,
) -> JsonObject:
    exact_family, exact_groups = _family_from_exact(exact_report)
    concept_family, concept_groups = _family_from_concept(concept_report)
    real = aggregate_real_query(rows)
    return {
        "schema": "aft-search-score-v1",
        "evidence_sha": EVIDENCE_SHA,
        "model_id": model_id,
        "profile": profile,
        "capability": dict(capability),
        "manifest_path": _display_path(manifest_path),
        "manifest_sha256": sha256_file(manifest_path),
        "baseline_path": _display_path(reference),
        "baseline_sha256": sha256_file(reference) if reference.is_file() else None,
        "binary_sha256": sha256_file(binary),
        "families": {
            "exact_recall": exact_family,
            "concept_recall": concept_family,
            "real_query": real["family"],
        },
        "fixture_groups": {"exact_recall": exact_groups, "concept_recall": concept_groups},
        "shapes": real["shapes"],
        "mechanisms": real["mechanisms"],
        "census_weighted_mrr_report_only": real["census_weighted_mrr_report_only"],
        "fixture_results": {"harness-goldens": True, "profile-grammar": True, "paging": profile != "paged" or capability.get("probe_pages_differ") is True},
        "rows": [dict(row) for row in rows],
    }


def load_inputs(manifest_path: Path) -> tuple[JsonObject, Path, Path, JsonObject]:
    manifest = json.loads(manifest_path.read_text())
    if not isinstance(manifest, dict) or manifest.get("evidence_sha") != EVIDENCE_SHA:
        raise InputFault("corpus_vector_model_mismatch:manifest")
    included = [row for row in manifest.get("rows", []) if "excluded_reason" not in row]
    if not included:
        raise InputFault("empty_population")
    bundles = {str(row.get("bundle")) for row in included}
    packs = {str(row.get("embedding_pack")) for row in included}
    bundle_digests = {str(row.get("bundle_sha256")) for row in included}
    pack_digests = {str(row.get("embedding_pack_sha256")) for row in included}
    if len(bundles) != 1 or len(packs) != 1 or len(bundle_digests) != 1 or len(pack_digests) != 1:
        raise InputFault("corpus_vector_model_mismatch:row_bindings")
    bundle = ROOT / next(iter(bundles))
    pack_path = ROOT / next(iter(packs))
    if not bundle.is_file() or sha256_file(bundle) != next(iter(bundle_digests)):
        raise InputFault(f"corpus_vector_model_mismatch:{_display_path(bundle)}")
    if not pack_path.is_file() or sha256_file(pack_path) != next(iter(pack_digests)):
        raise InputFault(f"corpus_vector_model_mismatch:{_display_path(pack_path)}")
    pack = json.loads(pack_path.read_text())
    if pack.get("pinned_sha") != EVIDENCE_SHA or pack.get("schema") != "aft-search-vector-pack-v1":
        raise InputFault("corpus_vector_model_mismatch:embedding_pack")
    return manifest, bundle, pack_path, pack


@contextmanager
def materialized_bundle(bundle: Path) -> Iterator[Path]:
    with tempfile.TemporaryDirectory(prefix="aft-real-query-tree-") as directory:
        root = Path(directory) / "tree"
        root.mkdir()
        with zipfile.ZipFile(bundle) as archive:
            for info in archive.infolist():
                target = (root / info.filename).resolve()
                if root.resolve() not in target.parents or info.is_dir() or (info.external_attr >> 16) & 0o170000 == 0o120000:
                    raise InputFault(f"invalid_bundle_member:{info.filename}")
            archive.extractall(root)
        yield root


@contextmanager
def fixture_endpoint(pack: Mapping[str, Any], log_path: Path) -> Iterator[str]:
    vectors = pack.get("vectors")
    template = pack.get("embed_template_version")
    if not isinstance(vectors, dict) or not isinstance(template, str) or not vectors:
        raise InputFault("corpus_vector_model_mismatch:embedding_pack")
    server = Server(("127.0.0.1", 0), vectors, template, log_path)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--manifest", default=str(HERE / "real-query-manifest.json"))
    result.add_argument("--profile", choices=("single_page", "paged"), default="single_page")
    result.add_argument("--binary", default=DEFAULT_BINARY)
    result.add_argument("--schema", default=str(DEFAULT_SCHEMA))
    result.add_argument("--exact-score", default=str(HERE / ".bench/search-quality/exact.json"))
    result.add_argument("--concept-score", default=str(HERE / ".bench/search-quality/concept.json"))
    result.add_argument("--reference", default=str(HERE / "real-query-baseline.json"))
    result.add_argument("--output", default=str(HERE / ".bench/search-quality/score.json"))
    result.add_argument("--ready-timeout", type=float, default=600.0)
    return result


def ensure_binary(binary: Path) -> None:
    if binary.is_file():
        return
    if os.environ.get("AFT_BINARY_PATH") or binary != (ROOT / "target/release/aft").resolve():
        raise InputFault(f"aft_binary_missing:{binary}")
    result = subprocess.run(["cargo", "build", "--release", "-p", "agent-file-tools"], cwd=ROOT, check=False)
    if result.returncode or not binary.is_file():
        raise InputFault(f"aft_binary_build_failed:{result.returncode}")


def run(args: argparse.Namespace) -> int:
    manifest_path = Path(args.manifest).resolve()
    binary = Path(args.binary).resolve()
    ensure_binary(binary)
    manifest, bundle, _pack_path, pack = load_inputs(manifest_path)
    capability = load_capability(Path(args.schema).resolve())
    exact_report = json.loads(Path(args.exact_score).read_text())
    concept_report = json.loads(Path(args.concept_score).read_text())
    with tempfile.TemporaryDirectory(prefix="aft-real-query-run-") as run_dir, materialized_bundle(bundle) as project_root:
        runtime = Path(run_dir)
        log_path = runtime / "embedding-requests.log"
        stderr_path = runtime / "aft.stderr"
        with fixture_endpoint(pack, log_path) as endpoint:
            client = NdjsonClient(binary, project_root, runtime / "storage", stderr_path)
            try:
                client.configure(endpoint, str(pack["model_id"]), args.ready_timeout)
                client.wait_ready(args.ready_timeout)
                rows = score_manifest_rows(manifest, args.profile, capability, client, project_root)
            finally:
                client.close()
    score = assemble_score(
        manifest,
        rows,
        args.profile,
        capability,
        str(pack["model_id"]),
        exact_report,
        concept_report,
        manifest_path,
        binary,
        Path(args.reference).resolve(),
    )
    validate_scored_population(manifest, score)
    validate_profile_score(score)
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(canonical_json(score))
    print(f"real_query_rows:{len(rows)}")
    print(f"real_query_score:{output}")
    print(f"real_query_score_sha256:{sha256_file(output)}")
    return 0


def main() -> int:
    try:
        return run(parser().parse_args())
    except (AftProtocolError, InputFault, OSError, KeyError, ValueError, json.JSONDecodeError, zipfile.BadZipFile) as error:
        print(str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
