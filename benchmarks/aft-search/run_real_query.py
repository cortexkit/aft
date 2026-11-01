#!/usr/bin/env python3
"""Replay the checked-in real-query manifest through standalone AFT."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from contextlib import contextmanager, nullcontext
from pathlib import Path
from typing import Any, Iterator, Mapping, Optional, Sequence

import bench_rerank
from embedding_fixture_server import Server
from evidence_tree import evidence_tree_sha256
from ndjson_stream import NdjsonStream
from provision_evidence import evidence_root
from run import strip_verbatim_prefix
from search_quality_lib import (
    EVIDENCE_SHA,
    INVARIANCE_DEPTH,
    METRICS,
    PAGE_INVARIANCE_FAILED_FIELD,
    SEMANTIC_BUILDING_NOT_APPLICABLE_FIELD,
    InputFault,
    aggregate_real_query,
    canonical_json,
    choose_stop,
    collapse_paths,
    excused_page_invariance,
    episode_number,
    invariance_requests,
    mean_metrics,
    profile_requests,
    row_metrics,
    sha256_file,
    split_rows_not_applicable,
    validate_profile_score,
    validate_scored_population,
)
from vector_pack import read_pack

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
DEFAULT_BINARY = os.environ.get("AFT_BINARY_PATH", str(ROOT / "target/release/aft"))
DEFAULT_SCHEMA = ROOT / "packages/pi-plugin/src/tools/semantic.ts"
PROBE_TEXT = "semantic index fingerprint probe"
# Exit code reserved for "this platform cannot evaluate the reference pair", so
# a caller can tell it apart from an ordinary input fault (2).
PLATFORM_UNSUPPORTED_EXIT = 3
# The provider model name the harness configures for the fixture server. AFT
# counts calls to this name as fixture traffic rather than live model calls
# (search_b2::embed_counter::FIXTURE_PROVIDER_MODEL), so it stays fixed even
# though the vectors behind it are real all-MiniLM-L6-v2 output. The score's
# model_id names the pack's model instead.
FIXTURE_PROVIDER_MODEL = "aft-search-fixture-v1"
# A live-model replay differs from the pack only in who computes the vectors,
# but it is not deterministic, so its score must never be taken for a pack score.
LIVE_MODEL_SUFFIX = ":live"
JsonObject = dict[str, Any]


class AftProtocolError(RuntimeError):
    """A standalone-AFT protocol failure."""


class UnsupportedPlatform(RuntimeError):
    """This platform cannot evaluate the Unix-captured reference pair."""


def assert_reference_platform(platform: Optional[str] = None) -> None:
    """Stop before the index build when the reference pair cannot be reproduced.

    The checked-in vector pack and baseline were captured on Unix, and the text
    AFT embeds bakes the OS-native relative path into every chunk header. A
    Windows run therefore hashes `tests\\fixtures\\...` where the pack holds
    `tests/fixtures/...`, and no vector in the pack can ever be found. Without
    this check the run spends the whole index build to fail on an opaque
    `vector_missing:<digest>` line that says nothing about the platform.
    """
    platform = platform or sys.platform
    if platform.startswith("win") or platform == "cygwin":
        raise UnsupportedPlatform(
            f"real_query_platform_unsupported:{platform}: the real-query reference pair "
            "(real-query-vectors.bin and real-query-baseline.json) is Unix-captured and "
            "cannot be evaluated on this platform. Run this gate on Linux or macOS; CI "
            "runs it on ubuntu-latest."
        )


class NdjsonClient:
    """Minimal client for configure and public tool_call requests."""

    def __init__(
        self,
        binary: Path,
        project_root: Path,
        storage_dir: Path,
        stderr_path: Path,
        model_env: Optional[Mapping[str, str]] = None,
        *,
        views_enabled: bool = False,
    ):
        env = os.environ.copy()
        # With views on, startup and configure must select the same store without
        # an environment override. Separate home/data roots keep the client whose
        # corpus embeddings are deliberately paused from borrowing the primary
        # client's stored vectors. Views-off setup is unchanged for reference replay.
        if views_enabled:
            env.pop("AFT_STORAGE_DIR", None)
            env.pop("AFT_CACHE_DIR", None)
            for name, directory in {
                "HOME": storage_dir / "home",
                "XDG_CONFIG_HOME": storage_dir / "config",
                "XDG_DATA_HOME": storage_dir / "data",
                "XDG_CACHE_HOME": storage_dir / "cache",
                "XDG_STATE_HOME": storage_dir / "state",
                "XDG_RUNTIME_DIR": storage_dir / "runtime",
            }.items():
                directory.mkdir(parents=True, exist_ok=True, mode=0o700)
                env[name] = str(directory)
            env["USERPROFILE"] = env["HOME"]
            storage_dir = storage_dir / "data" / "cortexkit" / "aft"
        else:
            env["AFT_STORAGE_DIR"] = str(storage_dir)
        # An empty model cache by default: the pack replay must never find a
        # local model to fall back on. The live-model replay passes the
        # pre-provisioned cache and runtime instead; the proxy below still
        # blocks any download, so a missing model fails instead of fetching.
        env["FASTEMBED_CACHE_DIR"] = str(storage_dir / "model-cache")
        if model_env:
            env.update(model_env)
        bench_rerank.apply_to_env(env)
        env["HTTP_PROXY"] = env["HTTPS_PROXY"] = env["ALL_PROXY"] = "http://127.0.0.1:9"
        env["NO_PROXY"] = "127.0.0.1,localhost,::1"
        env.setdefault("RUST_LOG", "warn")
        if views_enabled:
            env.pop("AFT_STORAGE_DIR", None)
            env.pop("AFT_CACHE_DIR", None)
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
        if self.proc.stdout is None:
            raise AftProtocolError("aft_protocol:pipes_unavailable")
        self._stream = NdjsonStream(self.proc.stdout)
        self._next_id = 0
        self.allow_building = False

    def close(self) -> None:
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)
        self._stderr.close()

    def call(
        self,
        command: str,
        params: Optional[Mapping[str, Any]] = None,
        timeout: float = 120.0,
        *,
        cap_timeout: bool = True,
    ) -> JsonObject:
        # Only extend ordinary RPC waits; readiness polling can opt out so its
        # status requests remain inside the caller's single deadline.
        if cap_timeout:
            timeout = max(timeout, float(os.environ.get("AFT_SEARCH_BENCH_RPC_TIMEOUT", "0")))
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
            frame = self._stream.read_frame(min(0.1, deadline - time.monotonic()))
            if frame is not None and str(frame.get("id")) == request_id:
                return frame
        raise AftProtocolError(f"aft_protocol:timeout:{command}:{self.stderr_text()}")

    def stderr_text(self) -> str:
        self._stderr.flush()
        position = self._stderr.tell()
        self._stderr.seek(0)
        text = self._stderr.read()[-4000:]
        self._stderr.seek(position)
        return text.strip()

    def configure(
        self,
        endpoint: Optional[str],
        model_id: str,
        timeout: float,
        *,
        views_enabled: bool = False,
    ) -> None:
        semantic: JsonObject = {
            "backend": "openai_compatible" if endpoint else "fastembed",
            "model": model_id,
            "timeout_ms": int(timeout * 1000),
            "query_timeout_ms": int(timeout * 1000),
            "max_batch_size": 64,
            "max_files": 20000,
        }
        if endpoint:
            semantic["base_url"] = endpoint
        doc = bench_rerank.apply_to_config({
            "indexes": {"trigram": True, "semantic": True, "callgraph": False},
            "semantic": semantic,
        })
        if views_enabled:
            doc["views"] = {"enabled": True}
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
        self.views_enabled = views_enabled
        if response.get("success") is not True:
            raise AftProtocolError(f"configure_failed:{response}")

    def views_work(self) -> JsonObject:
        """The standalone NDJSON status and health digest omit `views_work`."""
        return {
            "status": "unavailable",
            "reason": "standalone_ndjson_status_and_health_digest_do_not_expose_metrics.views_work",
        }

    def wait_ready(self, timeout: float, *, readiness_request: Optional[Mapping[str, Any]] = None) -> None:
        if self.views_enabled:
            if readiness_request is None:
                raise AftProtocolError("views_readiness_requires_public_request")
            deadline = time.monotonic() + timeout
            last: JsonObject = {}
            while time.monotonic() < deadline:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                status = self.call("status", timeout=remaining, cap_timeout=False)
                if "views" not in status:
                    raise AftProtocolError("views_runtime_not_enabled")
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                last = self.readiness_reply(readiness_request, remaining)
                if full_views_coverage(last):
                    return
                time.sleep(min(1.0, max(0.0, deadline - time.monotonic())))
            raise AftProtocolError(f"views_runtime_not_ready:timeout={timeout}:reply={last}")
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
                settled = bench_rerank.settle(lambda: self.call("status", timeout=30.0))
                if settled is not None:
                    print(f"semantic_settled:{json.dumps(settled, sort_keys=True)}", flush=True)
                return
            time.sleep(0.1)
        raise AftProtocolError(f"index_ready_timeout:{last}:{self.stderr_text()}")

    def readiness_reply(self, arguments: Mapping[str, Any], timeout: float) -> JsonObject:
        # A v3 lexical view is loaded by a public query, not by status polling.
        # Keep the reply intact: pending paths or any partial marker mean that
        # ready-row replay must wait, even when both resident indexes say ready.
        return self.call(
            "tool_call",
            {"session_id": "aft-search-real-query", "name": "search", "arguments": dict(arguments)},
            timeout=timeout,
            cap_timeout=False,
        )


    def search(self, arguments: Mapping[str, Any]) -> JsonObject:
        started = time.perf_counter()
        response = self.call(
            "tool_call",
            {"session_id": "aft-search-real-query", "name": "search", "arguments": dict(arguments)},
        )
        bench_rerank.observe("search", arguments, response, (time.perf_counter() - started) * 1000.0)
        allowed_statuses = {"ready", "building"} if self.allow_building else {"ready"}
        if response.get("success") is not True or response.get("status") not in allowed_statuses:
            if getattr(self, "views_enabled", False) and response.get("status") == "partial":
                try:
                    status_at_failure: JsonObject = self.call("status")
                except (AftProtocolError, OSError) as error:
                    status_at_failure = {"status": "unavailable", "reason": type(error).__name__}
                detail = {
                    "reply": response,
                    "status_at_failure": status_at_failure,
                }
                raise AftProtocolError(
                    f"aft_search_failed:views_partial:{json.dumps(detail, sort_keys=True)}"
                )
            raise AftProtocolError(f"aft_search_failed:success={response.get('success')}:status={response.get('status')}:code={response.get('code')}")
        if not isinstance(response.get("results"), list):
            raise AftProtocolError("aft_search_failed:results_not_array")
        return response


def full_views_coverage(reply: Mapping[str, Any]) -> bool:
    lanes = reply.get("lanes", {})
    if not isinstance(lanes, Mapping) or not all(isinstance(lanes.get(lane), Mapping) for lane in ("trigram", "semantic")):
        return False
    return (
        reply.get("success") is True
        and reply.get("status") == "ready"
        and reply.get("semantic_status") == "ready"
        and reply.get("complete") is True
        and isinstance(reply.get("results"), list)
        and bool(reply["results"])
        and not reply.get("fully_degraded", False)
        and not reply.get("omitted_lanes")
        and not reply.get("pending_paths")
        and not reply.get("partial", False)
        and lanes.get("trigram", {}).get("status") == "ready"
        and lanes.get("semantic", {}).get("status") == "ready"
        and not any(key in reply for key in ("semantic_gap", "index_read_gap", "exact_fallback_gap"))
    )


def building_views_lexical_ready(reply: Mapping[str, Any]) -> bool:
    # The semantic-building fixture pauses corpus embedding responses. Its
    # lexical index must answer before replay, but its semantic index must not
    # be misreported as ready while those responses are paused.
    lanes = reply.get("lanes", {})
    if not isinstance(lanes, Mapping) or not isinstance(lanes.get("trigram"), Mapping):
        return False
    return (
        reply.get("success") is True
        and reply.get("status") == "building"
        and reply.get("semantic_status") == "building"
        and lanes["trigram"].get("status") == "ready"
        and not reply.get("fully_degraded", False)
    )


def live_model_env() -> dict[str, str]:
    """The managed ONNX Runtime and model cache for a live-model replay.

    The client still points every proxy at a dead port, so a missing model
    fails the semantic index instead of being downloaded mid-run.
    """
    from run_prefrontal_search import ensure_local_model_env

    chosen = ensure_local_model_env()
    env = {
        "ORT_DYLIB_PATH": chosen.get("ort_dylib_path"),
        "FASTEMBED_CACHE_DIR": chosen.get("fastembed_cache_dir"),
    }
    return {key: str(value) for key, value in env.items() if value}


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
    try:
        value = json.loads(raw)
    except json.JSONDecodeError:
        # Renderer and implementation edits do not change the tool's parameters.
        # Preserve schema text exactly apart from platform-specific line endings.
        text = raw.decode("utf-8").replace("\r\n", "\n").replace("\r", "\n")
        block = _schema_block(text)
        digest = hashlib.sha256(block.encode("utf-8")).hexdigest()
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
    digest = hashlib.sha256(canonical_json(value)).hexdigest()
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
    raw = strip_verbatim_prefix(str(result.get("file", result.get("path", ""))))
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


def probe_pattern_capability(client: Any) -> JsonObject:
    """Use an invalid regex to distinguish parsing from silently ignored input."""
    response = client.call("tool_call", {"session_id": "aft-search-capability", "name": "search", "arguments": {
        "query": PROBE_TEXT, "pattern": "[", "topK": 1, "includeTests": False,
    }})
    error = response.get("error", {})
    code = response.get("code", response.get("error_code"))
    if isinstance(error, Mapping):
        code = error.get("code", code)
    if response.get("success") is False and code == "invalid_pattern":
        return {"pattern_declared": True, "pattern_probe": "invalid_pattern"}
    if response.get("success") is True:
        return {"pattern_declared": False, "pattern_probe": "ignored_pattern"}
    raise InputFault(f"pattern_capability_probe_failed:{response}")


def _request(arguments: Mapping[str, int], row: Mapping[str, Any], split: bool = False) -> JsonObject:
    request: JsonObject = {
        "query": row["query"],
        "topK": arguments["topK"],
        "includeTests": row["include_tests"],
    }
    if "pattern" in row:
        if split:
            request["pattern"] = row["pattern"]
        elif str(row["pattern"]).strip():
            request["query"] = row["query"] + " " + row["pattern"]
    if "offset" in arguments:
        request["offset"] = arguments["offset"]
    return request


def make_readiness_request(manifest: Mapping[str, Any]) -> JsonObject:
    row = next(row for row in manifest["rows"] if "excluded_reason" not in row)
    request = _request(profile_requests("single_page", False)[0], row)
    # Readiness must cover the entire checkout, not only the non-test subset
    # requested by some scored rows. A recorded query lets the fixture server
    # return its saved model embedding, without introducing unknown query text.
    request["includeTests"] = True
    return request


def _run_requests(client: Any, requests: Sequence[JsonObject], project_root: Path) -> tuple[list[JsonObject], list[JsonObject]]:
    responses: list[JsonObject] = []
    results: list[JsonObject] = []
    for request in requests:
        response = client.search(request)
        responses.append(response)
        results.extend(_normalize_results(response, project_root))
    return responses, results


def _invariance(client: Any, row: Mapping[str, Any], project_root: Path, split: bool = False) -> tuple[list[list[JsonObject]], list[str], bool]:
    """Run the invariance plans; return what was sent, the first plan's paths,
    and whether all plans collapsed to the same paths."""
    plans = invariance_requests()
    sent: list[list[JsonObject]] = []
    collapsed: list[list[str]] = []
    for plan in plans:
        requests = [_request(item, row, split) for item in plan]
        _, results = _run_requests(client, requests, project_root)
        sent.append(requests)
        collapsed.append(collapse_paths(results[:INVARIANCE_DEPTH]))
    return sent, collapsed[0], collapsed[0] == collapsed[1] == collapsed[2]


def score_manifest_rows(
    manifest: Mapping[str, Any],
    profile: str,
    capability: JsonObject,
    client: Any,
    project_root: Path,
    building_client: Any = None,
) -> list[JsonObject]:
    included = [row for row in manifest.get("rows", []) if "excluded_reason" not in row]
    if not included:
        raise InputFault("empty_population")
    plans = profile_requests(profile, bool(capability.get("offset_declared")))
    scored: list[JsonObject] = []
    probe_pages: Optional[list[list[JsonObject]]] = None
    for row in included:
        row_client = building_client if row.get("semantic_state") == "building" else client
        if row_client is None:
            raise InputFault(f"building_fixture_client_missing:{row['episode_id']}")
        split = capability.get("pattern_declared") is True
        requests = [_request(item, row, split) for item in plans]
        responses, results = _run_requests(row_client, requests, project_root)
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
        invariance_failure: Optional[str] = None
        if profile == "paged":
            invariance_sent, invariant_paths, invariant = _invariance(row_client, row, project_root, split)
            if not invariant:
                invariance_failure = f"page_invariance_failed:{row['episode_id']}"
            elif invariant_paths != collapse_paths(results[:INVARIANCE_DEPTH]):
                invariance_failure = f"page_invariance_failed:{row['episode_id']}:scoring"
            # A row whose manifest entry carries reference_not_page_invariant
            # is recorded as a miss instead of raising, so a reference can be
            # taken on an engine with the defect. validate_candidate_page_invariance
            # in search_quality_lib still refuses that row when this score is
            # evaluated as a candidate.
            if invariance_failure and not excused_page_invariance(row):
                raise InputFault(invariance_failure)
        if invariance_failure is None:
            metrics = row_metrics(ranked_paths, str(row["opened_file"]))
        else:
            # A ranking that changes with page size has no one order to
            # record, and replays of it differ, so the miss keeps no paths.
            metrics = {metric: 0.0 for metric in METRICS}
            ranked_paths, retrieval_depth, page_zero_ranked_paths = [], 0, []
        paired: JsonObject = {}
        if "pattern" in row:
            prose_row = {key: value for key, value in row.items() if key != "pattern"}
            prose_requests = [_request(item, prose_row) for item in plans]
            _, prose_results = _run_requests(row_client, prose_requests, project_root)
            prose_paths = collapse_paths(prose_results)[:10]
            prose_metrics = row_metrics(prose_paths, str(row["opened_file"]))
            if split and row["split_kind"] == "R5" and results != prose_results:
                raise InputFault(f"empty_pattern_not_identical:{row['episode_id']}")
            paired = {
                "input_form": "split" if split else "joined",
                "answer_kind": row["answer_kind"],
                "split_kind": row["split_kind"],
                **({"pattern_summary": _relative_pattern_summary(responses[0].get("pattern_summary"), project_root), "summary_text": str(responses[0].get("text", "")).splitlines()[:8]} if split else {}),
                # Diagnostics only, present when the engine runs with
                # AFT_SEARCH_SPLIT_TRACE set: what placed each leading result.
                **({"split_trace": responses[0]["split_trace"]} if split and responses[0].get("split_trace") else {}),
                "prose_only": {"requests": prose_requests, "ranked_paths": prose_paths, "metrics": prose_metrics},
                "paired_mrr_delta": metrics["mrr_at_10"] - prose_metrics["mrr_at_10"],
                **({"semantic_state": "building", "envelope_complete": responses[0].get("complete")} if row.get("semantic_state") == "building" else {}),
            }
            if profile == "paged":
                _, _, prose_invariant = _invariance(row_client, prose_row, project_root)
                if not prose_invariant:
                    raise InputFault(f"prose_page_invariance_failed:{row['episode_id']}")
        scored.append(
            {
                "episode_id": row["episode_id"],
                **paired,
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
                **({PAGE_INVARIANCE_FAILED_FIELD: invariance_failure} if invariance_failure else {}),
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
    score: JsonObject = {
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
        "fixture_groups": {"exact_recall": exact_groups, "concept_recall": concept_groups,
            **({"real_query": {"split": mean_metrics([row["metrics"] for row in rows if "input_form" in row])}} if any("input_form" in row for row in rows) else {})},
        "paired_deltas": {row["episode_id"]: row["paired_mrr_delta"] for row in rows if "input_form" in row},
        "shapes": real["shapes"],
        "mechanisms": real["mechanisms"],
        "census_weighted_mrr_report_only": real["census_weighted_mrr_report_only"],
        "fixture_results": {"harness-goldens": True, "profile-grammar": True, "paging": profile != "paged" or capability.get("probe_pages_differ") is True},
        "rows": [dict(row) for row in rows],
    }
    # Say which split rows the gate will not judge on this engine, and why,
    # in the score itself rather than leaving the skip implicit.
    not_applicable = split_rows_not_applicable(score)
    if not_applicable:
        score["split_rows_not_applicable"] = not_applicable
    return score


def load_manifest_and_tree(
    manifest_path: Path, project_root: Optional[Path] = None
) -> tuple[JsonObject, Path, Path, str]:
    """Validate the manifest and pinned tree; return the pack path and its bound digest."""
    manifest = json.loads(manifest_path.read_text())
    if not isinstance(manifest, dict) or manifest.get("evidence_sha") != EVIDENCE_SHA:
        raise InputFault("corpus_vector_model_mismatch:manifest")
    included = [row for row in manifest.get("rows", []) if "excluded_reason" not in row]
    if not included:
        raise InputFault("empty_population")
    packs = {str(row.get("embedding_pack")) for row in included}
    tree_digests = {str(row.get("evidence_tree_sha256")) for row in included}
    pack_digests = {str(row.get("embedding_pack_sha256")) for row in included}
    if len(packs) != 1 or len(tree_digests) != 1 or len(pack_digests) != 1:
        raise InputFault("corpus_vector_model_mismatch:row_bindings")
    tree = project_root or evidence_root(EVIDENCE_SHA)
    try:
        tree_digest = evidence_tree_sha256(tree)
    except (FileNotFoundError, OSError, ValueError):
        tree_digest = None
    if tree_digest != next(iter(tree_digests)):
        raise InputFault(f"corpus_vector_model_mismatch:{_display_path(tree)}")
    return manifest, tree, ROOT / next(iter(packs)), next(iter(pack_digests))


def load_inputs(
    manifest_path: Path, project_root: Optional[Path] = None
) -> tuple[JsonObject, Path, Path, JsonObject]:
    manifest, tree, pack_path, pack_digest = load_manifest_and_tree(manifest_path, project_root)
    if not pack_path.is_file() or sha256_file(pack_path) != pack_digest:
        raise InputFault(f"corpus_vector_model_mismatch:{_display_path(pack_path)}")
    pack = read_pack(pack_path)
    if pack.get("pinned_sha") != EVIDENCE_SHA:
        raise InputFault("corpus_vector_model_mismatch:embedding_pack")
    supplement = manifest.get("split_query_pack")
    if supplement:
        supplemental_path = ROOT / supplement["path"]
        if sha256_file(supplemental_path) != supplement["sha256"]:
            raise InputFault("corpus_vector_model_mismatch:split_query_pack")
        extra = read_pack(supplemental_path)
        for field in ("pinned_sha", "model_id", "embed_template_version", "dimension"):
            if extra.get(field) != pack.get(field):
                raise InputFault(f"corpus_vector_model_mismatch:split_query_pack:{field}")
        vectors = dict(pack["vectors"])
        for key, vector in extra["vectors"].items():
            if key in vectors and vectors[key] != vector:
                raise InputFault("corpus_vector_model_mismatch:split_query_pack:overlap")
            vectors[key] = vector
        pack["vectors"] = vectors
    return manifest, tree, pack_path, pack


@contextmanager
def runtime_evidence_tree(tree: Path, *, views_enabled: bool = False) -> Iterator[Path]:
    """Copy the verified tree outside its parent checkout before starting AFT."""
    with tempfile.TemporaryDirectory(prefix="aft-real-query-tree-") as directory:
        root = Path(directory) / "tree"
        shutil.copytree(tree, root, ignore=shutil.ignore_patterns(".git"))
        if views_enabled:
            attach_pinned_git_metadata(root)
        copy_answer_key_ignore(root)
        copy_evidence_root_ignore(root)
        yield root


def attach_pinned_git_metadata(root: Path) -> None:
    """Expose the pinned commit's tree to content-addressed views in the copy."""
    objects_result = subprocess.run(
        ["git", "rev-parse", "--git-path", "objects"],
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=False,
    )
    if objects_result.returncode:
        detail = objects_result.stderr.strip()
        raise InputFault(f"views_git_objects_unavailable:{detail}")
    objects_path = Path(objects_result.stdout.strip())
    if not objects_path.is_absolute():
        objects_path = ROOT / objects_path
    objects_path = objects_path.resolve()

    init_result = subprocess.run(
        ["git", "init", "--quiet", str(root)],
        cwd=ROOT,
        text=True,
        capture_output=True,
        check=False,
    )
    if init_result.returncode:
        detail = init_result.stderr.strip()
        raise InputFault(f"views_git_init_failed:{detail}")
    alternates = root / ".git" / "objects" / "info" / "alternates"
    alternates.parent.mkdir(parents=True, exist_ok=True)
    alternates.write_text(f"{objects_path}\n", encoding="utf-8")
    head_result = subprocess.run(
        ["git", "update-ref", "--no-deref", "HEAD", EVIDENCE_SHA],
        cwd=root,
        text=True,
        capture_output=True,
        check=False,
    )
    if head_result.returncode:
        detail = head_result.stderr.strip()
        raise InputFault(f"views_git_head_unavailable:{detail}")


# Where the answer-key ignore list is copied inside the evidence tree, which is
# a projection of this repository and so has the same relative layout.
BENCH_IGNORE_RELATIVE = "benchmarks/aft-search/.aftignore"

# First line of the copy's root .aftignore. The default evidence copy has no .git;
# views-enabled copies attach the pinned commit's metadata. AFT applies .gitignore
# files and the machine's global git excludes file whether or not the copy is a
# Git repository. The benchmark's candidate pool is the tracked tree, including
# files that were force-added despite matching a .gitignore (plans and reports
# under .alfonso/, benchmark results), so the root .aftignore re-includes every
# entry before its own exclusions. An .aftignore outranks .gitignore and the
# global excludes file, later lines in the same file still win, and the deeper
# benchmarks/aft-search/.aftignore is consulted first, so every exclusion the
# benchmark writes still applies.
#
# The pattern re-includes only names that do not start with a dot. A
# re-included entry also bypasses the hidden-file filter, so `!*` would add
# the hidden directories (.gsd/, .github/, .alfonso/, ...) to walks that skip
# hidden files, such as the call graph's. Hidden names fall through to the
# usual rules, and no .gitignore in the pinned tree names a hidden entry
# itself, so both kinds of walk see exactly the pool they saw before.
EVIDENCE_POOL_WHITELIST = b"# Pin the pool to the tracked tree; see run_real_query.py.\n![!.]*\n"


def _relative_pattern_summary(summary: Any, project_root: Path) -> Any:
    """Rewrite the engine's absolute definition paths relative to the tree.

    The evidence tree is copied into a fresh temporary directory on every run,
    and that directory's name differs between runs and between platforms
    (`/tmp/...` on Linux, `/private/var/folders/...` on macOS). The pattern
    summary is part of each recorded row, so absolute paths there made two runs
    of an identical engine compare unequal.
    """
    roots = {str(project_root), str(project_root.resolve())}
    if isinstance(summary, dict):
        return {key: _relative_pattern_summary(value, project_root) for key, value in summary.items()}
    if isinstance(summary, list):
        return [_relative_pattern_summary(value, project_root) for value in summary]
    if isinstance(summary, str):
        for root in roots:
            if summary.startswith(root + os.sep):
                return summary[len(root) + 1 :].replace(os.sep, "/")
    return summary


def copy_answer_key_ignore(root: Path) -> Optional[Path]:
    """Keep the benchmark's own fixtures out of the index AFT builds here.

    The pinned tree predates the ignore list and its digest is verified before
    this copy exists, so the list is added to the copy rather than to the tree
    itself. No real-query row shares a query with those fixtures today; adding
    the list keeps that true by construction instead of by coincidence.
    """
    source = HERE / ".aftignore"
    if not source.is_file():
        return None
    destination = root / BENCH_IGNORE_RELATIVE
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(source.read_bytes())
    return destination


# Ignore list for answer-key files that live outside this benchmark directory,
# such as the report under .alfonso/. An .aftignore only applies to paths below
# its own directory, so this list is written to the root of the evidence-tree
# copy instead of sitting next to this directory's own .aftignore.
EVIDENCE_ROOT_IGNORE_SOURCE = HERE / "evidence-root.aftignore"


def copy_evidence_root_ignore(root: Path) -> Path:
    """Keep answer-key files elsewhere in the tree out of the index AFT builds.

    The pinned tree has a report under `.alfonso/` that quotes the concept
    fixtures' queries; see `evidence-root.aftignore` for why it is excluded.
    The pinned tree has no root `.aftignore` today. If a later pin adds one,
    its entries are kept and this list is appended, so the tree's own
    exclusions still apply. `EVIDENCE_POOL_WHITELIST` goes first so that no
    `.gitignore` rule removes a tracked file from the pool.
    """
    destination = root / ".aftignore"
    existing = destination.read_bytes() if destination.is_file() else b""
    if existing and not existing.endswith(b"\n"):
        existing += b"\n"
    destination.write_bytes(
        EVIDENCE_POOL_WHITELIST + existing + EVIDENCE_ROOT_IGNORE_SOURCE.read_bytes()
    )
    return destination


@contextmanager
def fixture_endpoint(pack: Mapping[str, Any], log_path: Path, corpus_release: Optional[threading.Event] = None) -> Iterator[str]:
    vectors = pack.get("vectors")
    template = pack.get("embed_template_version")
    if not isinstance(vectors, Mapping) or not isinstance(template, str) or not vectors:
        raise InputFault("corpus_vector_model_mismatch:embedding_pack")
    server = Server(("127.0.0.1", 0), vectors, template, log_path, corpus_release=corpus_release, unheld_corpus_texts={PROBE_TEXT})
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}"
        if server.refused:
            raise InputFault(f"vector_missing:{server.refused[0]}")
    finally:
        if corpus_release is not None:
            corpus_release.set()
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--manifest", default=str(HERE / "real-query-manifest.json"))
    result.add_argument("--tuning-only", action="store_true", help="Report-only replay of the separate tuning manifest; never gate or record it.")
    result.add_argument("--profile", choices=("single_page", "paged"), default="single_page")
    result.add_argument("--binary", default=DEFAULT_BINARY)
    result.add_argument("--schema", default=str(DEFAULT_SCHEMA))
    result.add_argument("--exact-score", default=str(HERE / ".bench/search-quality/exact.json"))
    result.add_argument("--concept-score", default=str(HERE / ".bench/search-quality/concept.json"))
    result.add_argument("--reference", default=str(HERE / "real-query-baseline.json"))
    result.add_argument("--output", default=str(HERE / ".bench/search-quality/score.json"))
    result.add_argument("--ready-timeout", type=float, default=600.0)
    result.add_argument("--views", action="store_true", help="Enable content-addressed views and record AFT views-work counters.")
    result.add_argument(
        "--live-model",
        action="store_true",
        help=(
            "Report-only fidelity check: replay with AFT's own local backend (the managed ONNX "
            "Runtime and model cache) instead of the vector pack. Its model_id carries a "
            f"'{LIVE_MODEL_SUFFIX}' suffix so the gate refuses to compare it with the reference."
        ),
    )
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
    assert_reference_platform()
    manifest_path = Path(args.manifest).resolve()
    binary = Path(args.binary).resolve()
    ensure_binary(binary)
    manifest, provisioned_tree, _pack_path, pack = load_inputs(manifest_path)
    if bool(manifest.get("tuning_only")) != bool(args.tuning_only):
        raise InputFault("tuning_manifest_requires_tuning_only_flag")
    capability = load_capability(Path(args.schema).resolve())
    exact_report = json.loads(Path(args.exact_score).read_text())
    concept_report = json.loads(Path(args.concept_score).read_text())
    readiness_request = make_readiness_request(manifest) if args.views else None
    with tempfile.TemporaryDirectory(prefix="aft-real-query-run-") as run_dir, runtime_evidence_tree(
        provisioned_tree, views_enabled=args.views
    ) as project_root:
        runtime = Path(run_dir)
        log_path = runtime / "embedding-requests.log"
        stderr_path = runtime / "aft.stderr"
        if args.live_model:
            model_id = str(pack["model_id"]) + LIVE_MODEL_SUFFIX
            endpoint_context: Any = nullcontext(None)
            client_env: Optional[dict[str, str]] = live_model_env()
            configured_model = str(pack["model_id"])
        else:
            model_id = str(pack["model_id"])
            endpoint_context = fixture_endpoint(pack, log_path)
            client_env = None
            configured_model = FIXTURE_PROVIDER_MODEL
        with endpoint_context as endpoint:
            client = NdjsonClient(binary, project_root, runtime / "storage", stderr_path, client_env, views_enabled=args.views)
            views_work: JsonObject = {}
            try:
                client.configure(endpoint, configured_model, args.ready_timeout, views_enabled=args.views)
                client.wait_ready(args.ready_timeout, readiness_request=readiness_request)
                bench_rerank.report_warm_up("real_query", bench_rerank.warm_up(
                    lambda query: client.search({"query": query, "topK": 50, "includeTests": False}),
                    bench_rerank.burn_in_queries(),
                    args.ready_timeout,
                ))
                capability.update(probe_pattern_capability(client))
                has_building_rows = any(
                    row.get("semantic_state") == "building" for row in manifest["rows"]
                )
                building_rows = [
                    row for row in manifest["rows"]
                    if "excluded_reason" not in row and row.get("semantic_state") == "building"
                ]
                not_applicable_building_rows: list[JsonObject] = []
                if has_building_rows:
                    with fixture_endpoint(pack, runtime / "building-embeddings.log", threading.Event()) as building_endpoint:
                        building = NdjsonClient(binary, project_root, runtime / "building-storage", runtime / "building.stderr", views_enabled=args.views)
                        building.allow_building = True
                        try:
                            building.configure(
                                building_endpoint,
                                FIXTURE_PROVIDER_MODEL,
                                args.ready_timeout,
                                views_enabled=args.views,
                            )
                            deadline = time.monotonic() + args.ready_timeout
                            status: JsonObject = {}
                            while time.monotonic() < deadline:
                                if args.views:
                                    assert readiness_request is not None
                                    status = building.readiness_reply(readiness_request, deadline - time.monotonic())
                                    semantic_status = status.get("semantic_status")
                                    lexical_status = status.get("lanes", {}).get("trigram", {}).get("status")
                                else:
                                    status = building.call("status", timeout=120.0, cap_timeout=True)
                                    semantic_status = status.get("semantic_index", {}).get("status")
                                    lexical_status = status.get("search_index", {}).get("status")
                                if semantic_status in {"failed", "unavailable"}:
                                    raise InputFault(f"semantic_building_fixture_not_building:{semantic_status}")
                                if semantic_status == "ready":
                                    if args.views and full_views_coverage(status):
                                        reason = (
                                            "views-enabled semantic index was ready before the held-embedding "
                                            "fixture reached the building state"
                                        )
                                        not_applicable_building_rows = [
                                            {
                                                "episode_id": str(row["episode_id"]),
                                                "semantic_state": "building",
                                                "not_applicable_reason": reason,
                                            }
                                            for row in building_rows
                                        ]
                                        break
                                    if not args.views:
                                        raise InputFault(f"semantic_building_fixture_not_building:{semantic_status}")
                                if (args.views and building_views_lexical_ready(status)) or (
                                    not args.views and lexical_status == "ready" and (
                                        semantic_status == "building" or (semantic_status == "loading" and status.get("semantic_index", {}).get("stage") == "embedding_symbols")
                                    )
                                ):
                                    break
                                time.sleep(1.0 if args.views else 0.1)
                            else:
                                if args.views:
                                    reason = (
                                        "views-enabled semantic-building fixture did not reach the building "
                                        "state before its readiness timeout"
                                    )
                                    not_applicable_building_rows = [
                                        {
                                            "episode_id": str(row["episode_id"]),
                                            "semantic_state": "building",
                                            "not_applicable_reason": reason,
                                        }
                                        for row in building_rows
                                    ]
                                else:
                                    raise InputFault(f"semantic_building_fixture_timeout:lexical={status.get('search_index', {}).get('status')}:semantic={status.get('semantic_index', {}).get('status')}:stage={status.get('semantic_index', {}).get('stage')}")
                            if not_applicable_building_rows:
                                scoring_manifest = dict(manifest)
                                scoring_manifest["rows"] = [
                                    row for row in manifest["rows"]
                                    if row not in building_rows
                                ]
                                rows = score_manifest_rows(
                                    scoring_manifest, args.profile, capability, client, project_root
                                )
                            else:
                                rows = score_manifest_rows(manifest, args.profile, capability, client, project_root, building)
                            if args.views:
                                views_work["building"] = building.views_work()
                        finally:
                            if args.views and "building" not in views_work:
                                views_work["building"] = building.views_work()
                            building.close()
                else:
                    rows = score_manifest_rows(manifest, args.profile, capability, client, project_root)
                if args.views:
                    views_work["primary"] = client.views_work()
            finally:
                if args.views:
                    if "primary" not in views_work:
                        views_work["primary"] = client.views_work()
                    print(f"views_work:{json.dumps(views_work, sort_keys=True)}")
                client.close()
    score = assemble_score(
        manifest,
        rows,
        args.profile,
        capability,
        model_id,
        exact_report,
        concept_report,
        manifest_path,
        binary,
        Path(args.reference).resolve(),
    )
    if args.views:
        score["views_enabled"] = True
        score["views_work"] = views_work
    if not_applicable_building_rows:
        score["rows"].extend(not_applicable_building_rows)
        manifest_order = {
            str(row["episode_id"]): index for index, row in enumerate(manifest["rows"])
        }
        score["rows"].sort(key=lambda row: manifest_order[str(row["episode_id"])])
        score[SEMANTIC_BUILDING_NOT_APPLICABLE_FIELD] = {
            "reason": not_applicable_building_rows[0]["not_applicable_reason"],
            "rows": sorted(
                (row["episode_id"] for row in not_applicable_building_rows),
                key=episode_number,
            ),
        }
    if args.tuning_only:
        score["tuning_only"] = True
    else:
        validate_scored_population(manifest, score)
    validate_profile_score(score)
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(canonical_json(score))
    for row in rows:
        if "input_form" in row:
            print(f"split_pair:{row['episode_id']} form={row['input_form']} mrr_delta={row['paired_mrr_delta']:+.6f}")
            if row["input_form"] == "split":
                print(f"split_summary:{row['episode_id']} {row.get('pattern_summary')} {row.get('summary_text')}")
    print(f"real_query_rows:{len(rows)}")
    if "split_rows_not_applicable" in score:
        skipped = score["split_rows_not_applicable"]
        print(f"split_rows_not_applicable:{','.join(skipped['rows'])}:{skipped['reason']}")
    if SEMANTIC_BUILDING_NOT_APPLICABLE_FIELD in score:
        skipped = score[SEMANTIC_BUILDING_NOT_APPLICABLE_FIELD]
        print(f"semantic_building_rows_skipped:{','.join(skipped['rows'])}:{skipped['reason']}")
    print(f"real_query_score:{output}")
    print(f"real_query_score_sha256:{sha256_file(output)}")
    return 0


def main() -> int:
    try:
        return run(parser().parse_args())
    except UnsupportedPlatform as error:
        print(str(error), file=sys.stderr)
        return PLATFORM_UNSUPPORTED_EXIT
    except (AftProtocolError, InputFault, OSError, KeyError, ValueError, json.JSONDecodeError) as error:
        print(str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
