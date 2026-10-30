#!/usr/bin/env python3
"""Deterministic, offline primitives for the aft_search quality benchmark.

This module intentionally uses only the Python standard library.  CI can therefore
validate benchmark inputs without installing packages or consulting a network.
"""

from __future__ import annotations

import copy
import difflib
import hashlib
import json
import math
import os
import re
import shutil
import socket
import struct
import subprocess
import tempfile
from collections import Counter, defaultdict
from dataclasses import dataclass
from decimal import Decimal, DecimalException, ROUND_HALF_EVEN
from pathlib import Path
from typing import Any, Callable, Iterable, Iterator, Mapping, Sequence

EVIDENCE_SHA = "30d4a64f99b3b15fd88be6cf962fff4b3fe5ea17"
PAGE_SIZE = 50
D_0 = 400
INVARIANCE_DEPTH = 100
STRATA = ("identifier", "code_literal", "short", "nl", "log_excerpt")
STRATUM_COUNTS = {"identifier": 2015, "code_literal": 1718, "short": 799, "nl": 1820, "log_excerpt": 117}
STRATUM_SHARES = {"identifier": 0.3115, "code_literal": 0.2656, "short": 0.1235, "nl": 0.2813, "log_excerpt": 0.0181}
QUERY_KINDS = ("Identifier", "Mixed", "ErrorCode", "Path", "Regex", "NaturalLanguage")
PINNED_SHAPES = ("identifier", "mixed", "error_code", "path", "regex", "natural_language")
MECHANISMS = {
    "not_a_search_failure": (2473, 0.3823, (4973, 3184, 9183)),
    "wrong_lane_nl": (789, 0.1219, (4173, 2299, 1776)),
    "index_stale_or_missing": (719, 0.1111, (14964, 14613, 15342)),
    "topk_cut": (552, 0.0854, (8886, 15391, 7656)),
    "phrase_present_not_surfaced": (545, 0.0843, (667, 7617, 15364)),
    "renamed_or_variant_token": (532, 0.0822, (5985, 13820, 20065)),
    "scope_mismatch": (475, 0.0734, (4885, 2893, 9536)),
    "other": (284, 0.0438, (17004, 10672, 18091)),
    "identifier_not_definition_first": (101, 0.0156, (14708, 17879, 20043)),
    "split_query_pattern_fusion": (0, 0.0, ()),
}
FIXTURE_IDS = tuple(f"followup-census:{number}" for _, _, suffixes in MECHANISMS.values() for number in suffixes)
EXCLUSION_REASONS = {
    "repo_unowned_or_unavailable", "opened_file_absent_at_sha", "worktree_only_path",
    "label_not_bundle_eligible", "include_tests_indeterminate",
}
STOP_REASONS = {"ten_files", "exhausted", "page_cap"}
METRICS = ("mrr_at_10", "hit_at_1", "hit_at_5")
RANKING_FENCE_PREFIXES = (
    "crates/aft/src/commands/semantic_search.rs",
    "crates/aft/src/commands/semantic_search/",
    "crates/aft/src/search_index.rs",
    "crates/aft/src/query_shape.rs",
    "crates/aft/src/semantic_index.rs",
    "crates/aft/src/embed/",
    "crates/aft/src/lib.rs",
    # The shape router, lane plans and token variants decide which lanes a
    # query runs and which candidates they produce, so a change there moves
    # search results just as a scoring change does.
    "crates/aft/src/search_b2/",
    # The semantic index chunks each file on the symbols parser.rs extracts:
    # its grammars, queries and extractors decide which symbols exist and
    # which lines each one spans, and those lines become the chunk text that
    # is embedded and ranked. A parser-only change therefore moves ranked rows.
    # symbols.rs stays out: it only defines the Symbol and Range types and
    # their serialization, and never decides where a range starts or ends.
    "crates/aft/src/parser.rs",
    # Only the plugin files that shape an aft_search request or its rendering
    # are ranking surface; the rest of each plugin (bash, read, edit, wakes)
    # cannot move a search score and must not demand a ranking descriptor.
    "packages/pi-plugin/src/tools/semantic.ts",
    "packages/pi-plugin/src/__tests__/semantic",
    "packages/opencode-plugin/src/tools/semantic.ts",
    "packages/opencode-plugin/src/__tests__/semantic",
    "benchmarks/aft-search/engine-fixtures/",
)
TOOL_SCHEMA_PATH = "crates/aft/src/subc_tool_schemas.json"
# Only these known tool entries can be excluded from the search surface. New
# catalog keys remain shared until reviewed, rather than silently bypassing it.
NON_SEARCH_SCHEMA_ENTRIES = frozenset({
    "status", "bash", "powershell", "read", "write", "edit", "apply_patch",
    "grep", "glob", "outline", "zoom", "inspect", "callgraph", "conflicts",
    "ast_search", "ast_replace", "delete", "move", "import", "safety",
    "bash_status", "bash_kill", "bash_write",
})
# The parity cases and fixture-project data currently live inline in
# tool_call_parity_test.rs. Changes there or in future extracted fixture files
# count as parity fixture differences.
TOOL_CALL_PARITY_FIXTURE_SOURCE = "crates/aft/tests/integration/tool_call_parity_test.rs"
TOOL_CALL_PARITY_FIXTURE_PREFIX = "crates/aft/tests/fixtures/tool_call_parity/"
# Manifest field excusing one row's page-invariance failure in the reference
# only. The paged replay faults on any row whose ranking changes with page
# size. A row carrying this field (a non-empty reason naming the engine defect)
# is recorded instead as a miss with PAGE_INVARIANCE_FAILED_FIELD set, so a
# reference can be taken on an engine that has the defect. It never excuses a
# changed engine: under a ranking descriptor total_gate faults on any evaluated
# row that failed page invariance, flagged or not. Under any other descriptor
# the engine is unchanged, so a flagged row must reproduce the reference's
# recorded miss exactly. Set it only on rows added to measure that defect,
# never to quiet a row that used to pass.
REFERENCE_NOT_PAGE_INVARIANT_FIELD = "reference_not_page_invariant"
PAGE_INVARIANCE_FAILED_FIELD = "page_invariance_failed"


def excused_page_invariance(row: Mapping[str, Any]) -> bool:
    """True when a manifest row may be recorded despite failing page invariance."""
    reason = row.get(REFERENCE_NOT_PAGE_INVARIANT_FIELD)
    return isinstance(reason, str) and bool(reason.strip())

class InputFault(ValueError):
    """An input or harness fault which must take P1/exit 2."""


class RegressionError(RuntimeError):
    """A quality regression which must take P2/exit 1."""


def canonical_json(value: Any) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n").encode()


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def episode_number(episode_id: str) -> int:
    match = re.fullmatch(r"followup-census:([0-9]+)", episode_id)
    if not match or (match.group(1).startswith("0") and match.group(1) != "0"):
        raise InputFault(f"invalid_episode_id:{episode_id}")
    return int(match.group(1))


# Minimal BLAKE3 implementation supporting one 1024-byte chunk, which is sufficient for every sampling seed.
_IV = (0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19)
_MSG_PERMUTATION = (2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8)


def _rotr32(value: int, count: int) -> int:
    return ((value >> count) | (value << (32 - count))) & 0xFFFFFFFF


def _g(state: list[int], a: int, b: int, c: int, d: int, x: int, y: int) -> None:
    state[a] = (state[a] + state[b] + x) & 0xFFFFFFFF
    state[d] = _rotr32(state[d] ^ state[a], 16)
    state[c] = (state[c] + state[d]) & 0xFFFFFFFF
    state[b] = _rotr32(state[b] ^ state[c], 12)
    state[a] = (state[a] + state[b] + y) & 0xFFFFFFFF
    state[d] = _rotr32(state[d] ^ state[a], 8)
    state[c] = (state[c] + state[d]) & 0xFFFFFFFF
    state[b] = _rotr32(state[b] ^ state[c], 7)


def _compress(cv: Sequence[int], block: Sequence[int], counter: int, block_len: int, flags: int) -> list[int]:
    state = list(cv) + list(_IV[:4]) + [counter & 0xFFFFFFFF, counter >> 32, block_len, flags]
    schedule = list(range(16))
    for _ in range(7):
        _g(state, 0, 4, 8, 12, block[schedule[0]], block[schedule[1]])
        _g(state, 1, 5, 9, 13, block[schedule[2]], block[schedule[3]])
        _g(state, 2, 6, 10, 14, block[schedule[4]], block[schedule[5]])
        _g(state, 3, 7, 11, 15, block[schedule[6]], block[schedule[7]])
        _g(state, 0, 5, 10, 15, block[schedule[8]], block[schedule[9]])
        _g(state, 1, 6, 11, 12, block[schedule[10]], block[schedule[11]])
        _g(state, 2, 7, 8, 13, block[schedule[12]], block[schedule[13]])
        _g(state, 3, 4, 9, 14, block[schedule[14]], block[schedule[15]])
        schedule = [schedule[index] for index in _MSG_PERMUTATION]
    return [(state[i] ^ state[i + 8]) & 0xFFFFFFFF for i in range(8)] + [
        (state[i + 8] ^ cv[i]) & 0xFFFFFFFF for i in range(8)
    ]


def blake3(data: bytes) -> bytes:
    if len(data) > 1024:
        raise ValueError("benchmark seed input exceeds one BLAKE3 chunk")
    cv = list(_IV)
    blocks = [data[index:index + 64] for index in range(0, len(data), 64)] or [b""]
    for index, raw in enumerate(blocks):
        padded = raw + b"\0" * (64 - len(raw))
        words = struct.unpack("<16I", padded)
        flags = (1 if index == 0 else 0) | (2 if index == len(blocks) - 1 else 0)
        if index == len(blocks) - 1:
            output = _compress(cv, words, 0, len(raw), flags | 8)
            return struct.pack("<8I", *output[:8])
        cv = _compress(cv, words, 0, 64, flags)[:8]
    raise AssertionError("unreachable")


def _rotl32(value: int, count: int) -> int:
    return ((value << count) | (value >> (32 - count))) & 0xFFFFFFFF


def _quarter_round(state: list[int], a: int, b: int, c: int, d: int) -> None:
    state[a] = (state[a] + state[b]) & 0xFFFFFFFF; state[d] = _rotl32(state[d] ^ state[a], 16)
    state[c] = (state[c] + state[d]) & 0xFFFFFFFF; state[b] = _rotl32(state[b] ^ state[c], 12)
    state[a] = (state[a] + state[b]) & 0xFFFFFFFF; state[d] = _rotl32(state[d] ^ state[a], 8)
    state[c] = (state[c] + state[d]) & 0xFFFFFFFF; state[b] = _rotl32(state[b] ^ state[c], 7)


class ChaCha8:
    """ChaCha8 stream with a 32-byte key, zero nonce, and 32-bit block counter."""

    def __init__(self, key: bytes):
        if len(key) != 32:
            raise ValueError("ChaCha8 requires a 32-byte key")
        self.key = struct.unpack("<8I", key)
        self.counter = 0
        self.buffer = b""

    def _block(self) -> bytes:
        initial = list(struct.unpack("<4I", b"expand 32-byte k")) + list(self.key) + [self.counter, 0, 0, 0]
        state = initial.copy()
        for _ in range(4):
            _quarter_round(state, 0, 4, 8, 12); _quarter_round(state, 1, 5, 9, 13)
            _quarter_round(state, 2, 6, 10, 14); _quarter_round(state, 3, 7, 11, 15)
            _quarter_round(state, 0, 5, 10, 15); _quarter_round(state, 1, 6, 11, 12)
            _quarter_round(state, 2, 7, 8, 13); _quarter_round(state, 3, 4, 9, 14)
        self.counter = (self.counter + 1) & 0xFFFFFFFF
        return struct.pack("<16I", *((state[i] + initial[i]) & 0xFFFFFFFF for i in range(16)))

    def bytes(self, count: int) -> bytes:
        while len(self.buffer) < count:
            self.buffer += self._block()
        result, self.buffer = self.buffer[:count], self.buffer[count:]
        return result

    def randbelow(self, upper: int) -> int:
        if upper <= 0:
            raise ValueError("upper must be positive")
        limit = (1 << 64) - ((1 << 64) % upper)
        while True:
            value = int.from_bytes(self.bytes(8), "little")
            if value < limit:
                return value % upper


def seeded_order(ids: Iterable[str], manifest_seed: str, stratum: str) -> list[str]:
    """R35 order: numeric sort, then Fisher-Yates with the frozen seed encoding."""
    ordered = sorted(ids, key=episode_number)
    # Encoding is exactly UTF-8(seed) followed by UTF-8(stratum), with no delimiter.
    random = ChaCha8(blake3(manifest_seed.encode("utf-8") + stratum.encode("utf-8")))
    for index in range(len(ordered) - 1, 0, -1):
        other = random.randbelow(index + 1)
        ordered[index], ordered[other] = ordered[other], ordered[index]
    return ordered


def sample_plan(rows: Sequence[Mapping[str, Any]], manifest_seed: str, labels: set[str] | None = None) -> dict[str, Any]:
    by_stratum: dict[str, list[str]] = {name: [] for name in STRATA}
    for row in rows:
        stratum = str(row.get("census_stratum", ""))
        if stratum not in by_stratum:
            raise InputFault(f"unknown_census_stratum:{stratum}")
        episode_id = str(row["episode_id"])
        episode_number(episode_id)
        by_stratum[stratum].append(episode_id)
    if sum(len(values) for values in by_stratum.values()) != len({item for values in by_stratum.values() for item in values}):
        raise InputFault("duplicate_census_identity")
    plan_rows: list[dict[str, Any]] = []
    selected: list[str] = []
    labels = labels if labels is not None else {item for values in by_stratum.values() for item in values}
    for stratum in STRATA:
        shuffled = seeded_order(by_stratum[stratum], manifest_seed, stratum)
        initial = shuffled[:60]
        final: list[str] = []
        replacements: list[dict[str, str]] = []
        cursor = len(initial)
        for original in initial:
            if original in labels:
                final.append(original)
                continue
            while cursor < len(shuffled) and shuffled[cursor] not in labels:
                cursor += 1
            if cursor < len(shuffled):
                replacement = shuffled[cursor]; cursor += 1; final.append(replacement)
                replacements.append({"original": original, "replacement": replacement, "reason": "missing_label"})
            else:
                replacements.append({"original": original, "replacement": "", "reason": "label_replacement_exhausted"})
        selected.extend(final)
        plan_rows.append({
            "census_stratum": stratum,
            "eligible_count": len(shuffled),
            "seed_hex": blake3(manifest_seed.encode() + stratum.encode()).hex(),
            "initial_selection": initial,
            "final_selection": final,
            "under_populated": len(shuffled) if len(shuffled) < 60 else None,
            "replacements": replacements,
            "next_unselected": shuffled[cursor] if cursor < len(shuffled) else None,
            "shortfall": 60 - len(final) if len(final) < 60 else 0,
        })
    fixture_set = set(FIXTURE_IDS)
    union = sorted(set(selected) | fixture_set, key=episode_number)
    return {
        "schema": "aft-search-sample-plan-v1",
        "manifest_seed": manifest_seed,
        "seed_encoding": "utf8(manifest_seed)||utf8(census_stratum)",
        "hash": "BLAKE3-256",
        "rng": "ChaCha8/IETF-zero-nonce-counter0; uint64le rejection; descending Fisher-Yates",
        "stratum_traversal": list(STRATA),
        "target_per_stratum": 60,
        "strata": plan_rows,
        "sample_order": selected,
        "union_order": union,
        "dual_source": sorted(set(selected) & fixture_set, key=episode_number),
    }


def validate_census(rows: Sequence[Mapping[str, Any]], labels: Sequence[Mapping[str, Any]]) -> None:
    identities = [str(row["episode_id"]) for row in rows]
    if len(identities) != 6469 or len(set(identities)) != 6469:
        raise InputFault(f"census_identity_count:{len(set(identities))}:expected:6469")
    counts = Counter(str(row.get("census_stratum")) for row in rows)
    if dict(counts) != STRATUM_COUNTS:
        raise InputFault(f"census_stratum_counts:{dict(counts)}")
    label_counts = Counter(str(row.get("census_stratum")) for row in labels)
    if len(labels) != 300 or any(label_counts[name] != 60 for name in STRATA):
        raise InputFault(f"retained_label_counts:{dict(label_counts)}")
    if sum(value[0] for value in MECHANISMS.values()) != 6470:
        raise AssertionError("mechanism estimates are independently rounded to 6,470")


def collapse_paths(tuples: Sequence[Any], max_paths: int = 10) -> list[str]:
    paths: list[str] = []
    seen: set[str] = set()
    for item in tuples:
        path = item.rsplit("::", 1)[0] if isinstance(item, str) and "::" in item else (item if isinstance(item, str) else str(item.get("path", "")))
        if not path or path in seen:
            continue
        seen.add(path); paths.append(path)
        if len(paths) == max_paths:
            break
    return paths


def row_metrics(tuples: Sequence[Any], opened_file: str) -> dict[str, float]:
    paths = collapse_paths(tuples)
    try:
        rank = paths.index(opened_file) + 1
    except ValueError:
        rank = 0
    return {
        "mrr_at_10": (1.0 / rank if rank else 0.0),
        "hit_at_1": float(rank == 1),
        "hit_at_5": float(0 < rank <= 5),
    }


def mean_metrics(rows: Sequence[Mapping[str, float]]) -> dict[str, float]:
    if not rows:
        raise InputFault("empty_population")
    # fsum is correctly rounded and so independent of summation order. A plain
    # sum differs in the last bit depending on row order and Python build, and
    # the gate compares these aggregates exactly against the reference.
    return {metric: math.fsum(float(row[metric]) for row in rows) / len(rows) for metric in METRICS}


def aggregate_real_query(rows: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    if not rows:
        raise InputFault("empty_population")
    row_values = [row["metrics"] for row in rows]
    by_shape: dict[str, list[Mapping[str, float]]] = defaultdict(list)
    by_mechanism: dict[str, list[Mapping[str, float]]] = defaultdict(list)
    by_stratum: dict[str, list[Mapping[str, float]]] = defaultdict(list)
    for row in rows:
        by_shape[str(row["pinned_shape"])].append(row["metrics"])
        by_mechanism[str(row["mechanism"])].append(row["metrics"])
        by_stratum[str(row["census_stratum"])].append(row["metrics"])
    weighted_mrr = math.fsum(STRATUM_SHARES[name] * mean_metrics(values)["mrr_at_10"] for name, values in sorted(by_stratum.items()))
    return {
        "family": mean_metrics(row_values),
        "shapes": {name: mean_metrics(values) for name, values in sorted(by_shape.items())},
        "mechanisms": {name: mean_metrics(values) for name, values in sorted(by_mechanism.items())},
        "census_weighted_mrr_report_only": weighted_mrr,
    }


def parse_offset_capability(schema: Mapping[str, Any], *, schema_path: str, schema_sha256: str) -> dict[str, Any]:
    properties = schema.get("properties")
    if not isinstance(properties, Mapping):
        raise InputFault("capability_schema_invalid")
    offset = properties.get("offset")
    declared = isinstance(offset, Mapping)
    result: dict[str, Any] = {"schema_path": schema_path, "schema_sha256": schema_sha256, "offset_declared": declared}
    if declared:
        if offset.get("type") != "integer" or not isinstance(offset.get("minimum"), int) or not isinstance(offset.get("maximum"), int):
            raise InputFault("capability_schema_invalid:offset_bounds")
        result["offset_bounds"] = {"minimum": offset["minimum"], "maximum": offset["maximum"]}
    return result


def choose_stop(*, page_cap: bool, exhausted: bool, ten_files: bool) -> str:
    if page_cap:
        return "page_cap"
    if exhausted:
        return "exhausted"
    if ten_files:
        return "ten_files"
    raise InputFault("invalid_stop_fields")


def profile_requests(profile: str, offset_declared: bool) -> list[dict[str, int]]:
    if profile == "single_page":
        return [{"topK": PAGE_SIZE}]
    if profile == "paged":
        if not offset_declared:
            raise InputFault("illegal_profile:offset_not_declared")
        return [
            {"topK": PAGE_SIZE, "offset": offset}
            for offset in range(0, D_0, PAGE_SIZE)
        ]
    raise InputFault(f"illegal_profile:{profile}")


def invariance_requests() -> tuple[list[dict[str, int]], ...]:
    return (
        [{"topK": 10, "offset": offset} for offset in range(0, INVARIANCE_DEPTH, 10)],
        [{"topK": 25, "offset": offset} for offset in range(0, INVARIANCE_DEPTH, 25)],
        [
            {"topK": PAGE_SIZE, "offset": offset}
            for offset in range(0, INVARIANCE_DEPTH, PAGE_SIZE)
        ],
    )


def validate_profile_score(score: Mapping[str, Any]) -> None:
    profile = score.get("profile")
    capability = score.get("capability")
    if profile not in {"single_page", "paged"} or not isinstance(capability, Mapping):
        raise InputFault("illegal_profile")
    offset_declared = capability.get("offset_declared")
    if not isinstance(offset_declared, bool) or not capability.get("schema_path") or not re.fullmatch(r"[0-9a-f]{64}", str(capability.get("schema_sha256", ""))):
        raise InputFault("capability_schema_invalid")
    if offset_declared and capability.get("probe_pages_differ") is not True:
        raise InputFault("capability_probe_inconsistency")
    if not offset_declared and "probe_pages_differ" in capability:
        raise InputFault("capability_probe_inconsistency")
    for row in score.get("rows", []):
        if "not_applicable_reason" in row:
            continue
        requests = row.get("requests")
        if requests is None:
            request = row.get("request")
            requests = [request] if isinstance(request, Mapping) else []
        if not requests or any(not isinstance(request, Mapping) for request in requests):
            raise InputFault(f"request_bound_violation:{row.get('episode_id')}")
        if row.get("request") != requests[0]:
            raise InputFault(f"request_bound_violation:{row.get('episode_id')}:request_recorder")
        invariance = row.get("invariance_requests", [])
        if profile == "single_page":
            if len(requests) != 1 or requests[0].get("topK") != PAGE_SIZE or "offset" in requests[0] or invariance:
                raise InputFault(f"request_bound_violation:{row.get('episode_id')}:single_page")
        else:
            offsets = [request.get("offset") for request in requests]
            expected_offsets = list(range(0, D_0, PAGE_SIZE))
            if (
                not offset_declared
                or len(requests) != len(expected_offsets)
                or offsets != expected_offsets
                or any(request.get("topK") != PAGE_SIZE for request in requests)
            ):
                raise InputFault(f"request_bound_violation:{row.get('episode_id')}:paged")
            expected_invariance = invariance_requests()
            if not isinstance(invariance, list) or len(invariance) != 3:
                raise InputFault(f"request_bound_violation:{row.get('episode_id')}:invariance")
            for observed_plan, expected_plan in zip(invariance, expected_invariance):
                if not isinstance(observed_plan, list) or len(observed_plan) != len(expected_plan):
                    raise InputFault(f"request_bound_violation:{row.get('episode_id')}:invariance")
                grammar = [{key: request.get(key) for key in ("topK", "offset")} for request in observed_plan]
                if grammar != expected_plan:
                    raise InputFault(f"request_bound_violation:{row.get('episode_id')}:invariance")
        request_count = len(requests) + sum(len(plan) for plan in invariance)
        if row.get("pages_fetched") != len(requests) or row.get("request_count") != request_count:
            raise InputFault(f"request_bound_violation:{row.get('episode_id')}:request_count")
        all_requests = list(requests) + [request for plan in invariance for request in plan]
        if any(
            request.get("includeTests") is not row.get("request", {}).get("includeTests")
            or request.get("query") != row.get("request", {}).get("query")
            or request.get("pattern") != row.get("request", {}).get("pattern")
            for request in all_requests
        ):
            raise InputFault(f"replay_input_mismatch:{row.get('episode_id')}:request_set")
        if not isinstance(row.get("retrieval_depth"), int) or row["retrieval_depth"] < 0:
            raise InputFault(f"invalid_stop_fields:{row.get('episode_id')}:retrieval_depth")
        ranked_paths = row.get("ranked_paths")
        if not isinstance(ranked_paths, list) or len(ranked_paths) > 10 or len(ranked_paths) != len(set(ranked_paths)):
            raise InputFault(f"invalid_stop_fields:{row.get('episode_id')}:ranked_paths")


def validate_included_row_mechanisms(manifest: Mapping[str, Any]) -> None:
    rows = manifest.get("rows")
    if not isinstance(rows, list):
        raise InputFault("malformed_schema:manifest_rows")
    for row in rows:
        if "excluded_reason" in row:
            continue
        mechanism = row.get("mechanism")
        if not isinstance(mechanism, str) or mechanism not in MECHANISMS:
            raise InputFault(
                f"malformed_schema:mechanism:{row.get('episode_id')}:{mechanism}"
            )


def validate_manifest_relabels(
    old_manifest: Mapping[str, Any], new_manifest: Mapping[str, Any]
) -> None:
    old_rows = {row.get("episode_id"): row for row in old_manifest.get("rows", [])}
    for row in new_manifest.get("rows", []):
        old_row = old_rows.get(row.get("episode_id"))
        if old_row is None or old_row.get("mechanism") == row.get("mechanism"):
            continue
        reason = row.get("relabel_reason")
        if not isinstance(reason, str) or not reason.strip():
            raise InputFault(f"manifest_relabel_reason_missing:{row.get('episode_id')}")


def validate_manifest_maintenance_scores(
    old_manifest: Mapping[str, Any],
    new_manifest: Mapping[str, Any],
    old_score: Mapping[str, Any],
    new_score: Mapping[str, Any],
) -> None:
    """Refuse an old/new score pair that was not measured on one binary and profile.

    A manifest re-record compares the old manifest's score against the new
    one, so both must come from the same AFT binary and the same profile. The
    model may differ only when the manifest moved every included row to a new
    vector pack: a score's model_id names the pack's model, so a pack change
    changes it by design. Any other model difference means the two scores are
    not comparable.
    """
    old_binary = old_score.get("binary_sha256")
    if (
        not old_binary
        or old_binary != new_score.get("binary_sha256")
        or old_score.get("profile") != new_score.get("profile")
    ):
        raise InputFault("manifest_maintenance_binary_profile_mismatch")
    if old_score.get("model_id") == new_score.get("model_id"):
        return
    old_packs = {
        row.get("episode_id"): row.get("embedding_pack_sha256")
        for row in old_manifest.get("rows", [])
        if "excluded_reason" not in row
    }
    new_rows = [row for row in new_manifest.get("rows", []) if "excluded_reason" not in row]
    every_row_repacked = bool(new_rows) and all(
        row.get("episode_id") in old_packs
        and row.get("embedding_pack_sha256")
        and row.get("embedding_pack_sha256") != old_packs[row.get("episode_id")]
        for row in new_rows
    )
    if not every_row_repacked:
        raise InputFault("manifest_maintenance_binary_profile_mismatch:model_changed_without_pack_change")


def included_manifest_ids(manifest: Mapping[str, Any]) -> list[str]:
    if manifest.get("tuning_only"):
        raise InputFault("tuning_only_manifest_not_gate_eligible")
    rows = manifest.get("rows")
    if not isinstance(rows, list):
        raise InputFault("malformed_schema:manifest_rows")
    validate_included_row_mechanisms(manifest)
    ids = [str(row.get("episode_id", "")) for row in rows]
    duplicates = sorted(item for item, count in Counter(ids).items() if count > 1)
    if duplicates:
        raise InputFault(f"scored_population_mismatch:duplicate_manifest={duplicates}")
    result: list[str] = []
    for row in rows:
        episode_id = str(row.get("episode_id", "")); episode_number(episode_id)
        reason = row.get("excluded_reason")
        if reason is not None:
            if reason not in EXCLUSION_REASONS:
                raise InputFault(f"malformed_schema:excluded_reason:{reason}")
            continue
        result.append(episode_id)
    if not result:
        raise InputFault("empty_population")
    return result


def validate_scored_population(manifest: Mapping[str, Any], score: Mapping[str, Any]) -> None:
    if score.get("tuning_only"):
        raise InputFault("tuning_only_score_not_gate_eligible")
    expected = included_manifest_ids(manifest)
    score_rows = score.get("rows")
    if not isinstance(score_rows, list):
        raise InputFault("malformed_schema:score_rows")
    observed = [str(row.get("episode_id", "")) for row in score_rows]
    duplicates = sorted(item for item, count in Counter(observed).items() if count > 1)
    missing = sorted(set(expected) - set(observed), key=episode_number)
    foreign = sorted(set(observed) - set(expected))
    if duplicates or missing or foreign:
        raise InputFault(f"scored_population_mismatch:duplicate={duplicates}:missing={missing}:foreign={foreign}")
    not_applicable = semantic_building_rows_not_applicable(score)
    if not_applicable and score.get("views_enabled") is not True:
        raise InputFault(
            "semantic_building_rows_not_applicable_requires_views_enabled:true"
        )
    not_applicable_ids = set(not_applicable["rows"]) if not_applicable else set()
    building_ids = {
        str(row["episode_id"])
        for row in manifest.get("rows", [])
        if "excluded_reason" not in row and row.get("semantic_state") == "building"
    }
    if not_applicable_ids and not_applicable_ids != building_ids:
        raise InputFault(
            f"semantic_building_not_applicable_mismatch:declared={sorted(not_applicable_ids)}:"
            f"expected={sorted(building_ids)}"
        )
    for expected_id, row in zip(
        sorted(expected, key=episode_number),
        sorted(score_rows, key=lambda item: episode_number(str(item["episode_id"]))),
    ):
        if expected_id != row["episode_id"]:
            raise InputFault("scored_population_mismatch")
        manifest_row = next(item for item in manifest["rows"] if item["episode_id"] == expected_id)
        if "not_applicable_reason" in row:
            if (
                set(row) != {"episode_id", "semantic_state", "not_applicable_reason"}
                or row.get("semantic_state") != "building"
                or manifest_row.get("semantic_state") != "building"
                or not isinstance(row.get("not_applicable_reason"), str)
                or not row["not_applicable_reason"].strip()
            ):
                raise InputFault(f"malformed_semantic_building_not_applicable:{expected_id}")
            continue
        sent = row.get("request", {}).get("includeTests")
        if sent is not manifest_row.get("include_tests") or row.get("include_tests_source") != manifest_row.get("include_tests_source"):
            raise InputFault(
                f"replay_input_mismatch:{expected_id}:expected={manifest_row.get('include_tests')}/"
                f"{manifest_row.get('include_tests_source')}:sent={sent}"
            )
        if "pattern" in manifest_row:
            form = row.get("input_form")
            request = row.get("request", {})
            if form == "split":
                valid = score.get("capability", {}).get("pattern_declared") is True and request.get("query") == manifest_row["query"] and request.get("pattern") == manifest_row["pattern"]
            elif form == "joined":
                joined = manifest_row["query"] + (" " + manifest_row["pattern"] if manifest_row["pattern"].strip() else "")
                valid = score.get("capability", {}).get("pattern_declared") is False and request.get("query") == joined and "pattern" not in request
            else:
                valid = False
            if not valid or row.get("answer_kind") != manifest_row.get("answer_kind") or row.get("split_kind") != manifest_row.get("split_kind"):
                raise InputFault(f"split_input_form_mismatch:{expected_id}")
            if row.get("metrics") != row_metrics(row.get("ranked_paths", []), manifest_row["opened_file"]):
                raise InputFault(f"split_metric_mismatch:{expected_id}")
            paired = row.get("prose_only", {})
            paired_requests = paired.get("requests", [])
            expected_requests = [{key: value for key, value in item.items() if key != "pattern"} | {"query": manifest_row["query"]} for item in row.get("requests", [])]
            if paired_requests != expected_requests or paired.get("metrics") != row_metrics(paired.get("ranked_paths", []), manifest_row["opened_file"]):
                raise InputFault(f"split_prose_pair_mismatch:{expected_id}")
        stop = row.get("collapse_stop_reason")
        if stop not in STOP_REASONS:
            raise InputFault(f"invalid_stop_fields:{expected_id}:{stop}")
        if PAGE_INVARIANCE_FAILED_FIELD in row and not excused_page_invariance(manifest_row):
            raise InputFault(f"page_invariance_failed:{expected_id}:not_excused_by_manifest")
        if not isinstance(row.get("pages_fetched"), int) or row["pages_fetched"] < 1:
            raise InputFault(f"invalid_stop_fields:{expected_id}:pages_fetched")


def read_tool_schema_blobs(base: str | None, head: str, paths: Sequence[str], *, root: Path) -> tuple[bytes | None, bytes | None] | None:
    """Read committed blobs from the same merge-base diff used for path names."""
    if TOOL_SCHEMA_PATH not in paths:
        return None
    if not base:
        return (None, None)
    try:
        merge_base = subprocess.run(["git", "merge-base", base, head], cwd=root, capture_output=True, check=False)
        if merge_base.returncode:
            return (None, None)
        blobs = []
        for revision in (merge_base.stdout.decode().strip(), head):
            result = subprocess.run(["git", "show", f"{revision}:{TOOL_SCHEMA_PATH}"], cwd=root, capture_output=True, check=False)
            blobs.append(result.stdout if result.returncode == 0 else None)
        return (blobs[0], blobs[1])
    except (OSError, UnicodeError):
        return (None, None)


def _unique_json_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise ValueError("duplicate_key")
        value[key] = item
    return value


def _reject_json_constant(value: str) -> None:
    raise ValueError(f"non_json_constant:{value}")


def _search_schema_surface(blob: bytes | None, side: str) -> dict[str, Any]:
    if blob is None:
        raise InputFault(f"{side}:blob_unavailable")
    try:
        document = json.loads(blob, object_pairs_hook=_unique_json_object, parse_constant=_reject_json_constant, parse_float=Decimal)
    except (ValueError, UnicodeError, RecursionError, DecimalException) as error:
        raise InputFault(f"{side}:invalid_json") from error
    if not isinstance(document, dict) or not isinstance(document.get("search"), dict):
        raise InputFault(f"{side}:unrecognised_catalog")
    search = document["search"]
    if not ((search.get("type") == "object" and isinstance(search.get("properties"), dict)) or isinstance(search.get("$ref"), str)):
        raise InputFault(f"{side}:unrecognised_search_schema")
    if any(not isinstance(document[key], dict) for key in NON_SEARCH_SCHEMA_ENTRIES & document.keys()):
        raise InputFault(f"{side}:unrecognised_tool_entry")
    # Retain all shared metadata, including definitions and version fields. A
    # reference into another tool makes that entry part of the search surface too.
    surface = {key: value for key, value in document.items() if key not in NON_SEARCH_SCHEMA_ENTRIES}
    pending = list(surface.values())
    seen_refs: set[str] = set()
    while pending:
        value = pending.pop()
        if isinstance(value, list):
            pending.extend(value)
        elif isinstance(value, dict):
            pending.extend(value.values())
            for keyword in ("$ref", "$dynamicRef", "$recursiveRef"):
                if keyword not in value:
                    continue
                ref = value[keyword]
                if not isinstance(ref, str) or not ref.startswith("#/") or keyword != "$ref" or re.search(r"~(?![01])", ref):
                    raise InputFault(f"{side}:unsupported_schema_reference")
                if ref in seen_refs:
                    continue
                seen_refs.add(ref)
                parts = [part.replace("~1", "/").replace("~0", "~") for part in ref[2:].split("/")]
                target: Any = document
                try:
                    for part in parts:
                        if isinstance(target, list) and not re.fullmatch(r"0|[1-9][0-9]*", part):
                            raise ValueError("invalid_array_pointer")
                        target = target[int(part)] if isinstance(target, list) else target[part]
                except (KeyError, IndexError, TypeError, ValueError) as error:
                    raise InputFault(f"{side}:unresolved_schema_reference") from error
                key = parts[0]
                if key not in surface:
                    surface[key] = document[key]
                    pending.append(document[key])
    return surface


def _tool_schema_fenced(schema_blobs: tuple[bytes | None, bytes | None] | None, notes: list[str]) -> bool:
    try:
        if schema_blobs is None:
            raise InputFault("comparison_unavailable")
        base = _search_schema_surface(schema_blobs[0], "base")
        head = _search_schema_surface(schema_blobs[1], "head")
        fenced = not _same_json_value(base, head)
        reason = "search_or_shared_changed" if fenced else "other_tools_only_or_formatting"
    except (InputFault, RecursionError, UnicodeError) as error:
        fenced = True
        reason = f"fail_closed:{error}"
    notes.append(f"ranking_fence:{TOOL_SCHEMA_PATH}:{reason}")
    return fenced


def _same_json_value(left: Any, right: Any) -> bool:
    if isinstance(left, dict) and isinstance(right, dict):
        return left.keys() == right.keys() and all(_same_json_value(left[key], right[key]) for key in left)
    if isinstance(left, list) and isinstance(right, list):
        return len(left) == len(right) and all(_same_json_value(a, b) for a, b in zip(left, right))
    # JSON booleans are distinct from numbers, although Python's True equals 1.
    if isinstance(left, bool) or isinstance(right, bool):
        return type(left) is type(right) and left == right
    return left == right


def derive_slice_class(paths: Sequence[str], *, schema_blobs: tuple[bytes | None, bytes | None] | None = None, notes: list[str] | None = None) -> str:
    if not isinstance(paths, list) or any(not isinstance(path, str) or path.startswith("/") or ".." in Path(path).parts for path in paths):
        raise InputFault("unresolvable_diff")
    schema_fenced = TOOL_SCHEMA_PATH in paths and _tool_schema_fenced(schema_blobs, notes if notes is not None else [])
    return "ranking" if schema_fenced or any(any(path == prefix or path.startswith(prefix) for prefix in RANKING_FENCE_PREFIXES) for path in paths) else "non_ranking"


def validate_presentation_rows(descriptor: Mapping[str, Any]) -> None:
    """Allow only exact rendered-line transitions, never a ranking-field waiver."""
    if "presentation_rows" not in descriptor:
        return
    if descriptor.get("slice_class") != "engine_unwired":
        raise InputFault("malformed_descriptor:presentation_rows:engine_unwired_only")
    entries = descriptor["presentation_rows"]
    if not isinstance(entries, Mapping):
        raise InputFault("malformed_descriptor:presentation_rows:not_a_map")
    for episode_id, entry in entries.items():
        if not isinstance(episode_id, str):
            raise InputFault("malformed_descriptor:presentation_rows:episode_id")
        episode_number(episode_id)
        field = f"presentation_rows.{episode_id}"
        if not isinstance(entry, Mapping) or set(entry) != {"before", "after"}:
            raise InputFault(f"malformed_descriptor:{field}:keys_must_be_before_after")
        for side in ("before", "after"):
            lines = entry[side]
            if not isinstance(lines, list) or not all(isinstance(line, str) for line in lines):
                raise InputFault(f"malformed_descriptor:{field}.{side}:not_string_lines")


def resolve_descriptor(descriptor: Mapping[str, Any] | None, diff_paths: Sequence[str], *, schema_blobs: tuple[bytes | None, bytes | None] | None = None, notes: list[str] | None = None) -> tuple[dict[str, Any], bool]:
    derived = derive_slice_class(list(diff_paths), schema_blobs=schema_blobs, notes=notes)
    missing_ranking = descriptor is None and derived == "ranking"
    if descriptor is None:
        return {"slice_class": derived, "targeted_mechanism": "none", "kind": "harness", "fixtures": ["harness-goldens"]}, missing_ranking
    validate_presentation_rows(descriptor)
    declared = descriptor.get("slice_class")
    if declared == "engine_unwired":
        if derived != "ranking":
            raise InputFault(f"descriptor_class_mismatch:declared={declared}:derived={derived}")
        fixtures = descriptor.get("fixtures")
        if UNREACHABLE_SPLIT_ROWS_FIELD in descriptor:
            raise InputFault(f"malformed_descriptor:{UNREACHABLE_SPLIT_ROWS_FIELD}:ranking_only")
        if (
            descriptor.get("targeted_mechanism") != "none"
            or descriptor.get("kind") != "harness"
            or not isinstance(fixtures, list)
            or TOOL_CALL_PARITY_FIXTURE_SOURCE not in fixtures
        ):
            raise InputFault("malformed_descriptor:engine_unwired")
        return dict(descriptor), missing_ranking
    if declared != derived:
        raise InputFault(f"descriptor_class_mismatch:declared={declared}:derived={derived}")
    validate_unreachable_split_rows(descriptor)
    target = descriptor.get("targeted_mechanism")
    if derived == "ranking":
        if target == "none":
            if descriptor.get("kind") != "paging":
                raise InputFault("malformed_descriptor:latency_only_ranking")
        elif target not in MECHANISMS:
            raise InputFault("malformed_descriptor:targeted_mechanism")
    elif target != "none" or descriptor.get("kind") not in {"readiness", "paging", "confidence", "harness"}:
        raise InputFault("malformed_descriptor:non_ranking")
    fixtures = descriptor.get("fixtures", [])
    if derived == "non_ranking" and (not isinstance(fixtures, list) or not fixtures):
        raise InputFault("malformed_descriptor:fixtures")
    return dict(descriptor), missing_ranking


def _metric_block(block: Any, name: str) -> Mapping[str, float]:
    if not isinstance(block, Mapping):
        raise InputFault(f"missing_metric_block:{name}")
    for metric in METRICS:
        value = block.get(metric)
        if not isinstance(value, (int, float)) or isinstance(value, bool) or not math.isfinite(value) or not 0 <= value <= 1:
            raise InputFault(f"invalid_metric:{name}:{metric}")
    return block


def _first_row_difference(reference: Mapping[str, Any], score: Mapping[str, Any]) -> str | None:
    old_rows = reference.get("rows", [])
    new_rows = score.get("rows", [])
    if not isinstance(old_rows, list) or not isinstance(new_rows, list):
        return "real_query.rows"
    for index in range(max(len(old_rows), len(new_rows))):
        old = old_rows[index] if index < len(old_rows) else None
        new = new_rows[index] if index < len(new_rows) else None
        if canonical_json(old) != canonical_json(new):
            row = new if isinstance(new, Mapping) else old
            identity = row.get("episode_id") if isinstance(row, Mapping) else None
            return f"real_query.{identity or index}"
    return None


def _quantize_aggregate(value: Any) -> Any:
    if isinstance(value, Mapping):
        return {key: _quantize_aggregate(item) for key, item in value.items()}
    if isinstance(value, list):
        return [_quantize_aggregate(item) for item in value]
    if isinstance(value, float):
        return quantize6(value)
    return value


def _real_query_score_document(document: Mapping[str, Any]) -> dict[str, Any]:
    """Return behavior fields with derived floating-point aggregates normalized.

    Python 3.12 changed ``sum`` to use compensated summation, so aggregates made
    from byte-identical rows can differ from older Python results by one ULP.
    Rows remain exact; six decimal places remove that runtime-only difference
    from their derived summaries without weakening row-level comparisons.
    """
    score = dict(document)
    # split_rows_not_applicable is derived from the capability probe and the
    # rows, which are compared themselves; it is a reader-facing annotation.
    for key in ("schema", "baseline_path", "baseline_sha256", "binary_sha256", "fixture_groups", "rows", "split_rows_not_applicable"):
        score.pop(key, None)
    families = document.get("families", {})
    real_query = families.get("real_query") if isinstance(families, Mapping) else None
    score["families"] = {"real_query": _quantize_aggregate(real_query)}
    for key in ("shapes", "mechanisms", "census_weighted_mrr_report_only"):
        if key in score:
            score[key] = _quantize_aggregate(score[key])
    return score


def _real_query_score_bytes(document: Mapping[str, Any]) -> bytes:
    """Return canonical behavior bytes after removing recorder-only provenance."""
    return canonical_json(_real_query_score_document(document))


def _first_value_difference(reference: Any, score: Any, path: str = "") -> str | None:
    if isinstance(reference, Mapping) and isinstance(score, Mapping):
        for key in sorted(set(reference) | set(score)):
            child = f"{path}.{key}" if path else str(key)
            if key not in reference or key not in score:
                return child
            difference = _first_value_difference(reference[key], score[key], child)
            if difference:
                return difference
        return None
    if isinstance(reference, list) and isinstance(score, list):
        for index in range(max(len(reference), len(score))):
            child = f"{path}[{index}]"
            if index >= len(reference) or index >= len(score):
                return child
            difference = _first_value_difference(reference[index], score[index], child)
            if difference:
                return difference
        return None
    return path if canonical_json(reference) != canonical_json(score) else None


def real_query_behavior_diff(reference: Mapping[str, Any], score: Mapping[str, Any]) -> str:
    """Build an artifact-friendly diff of rows and normalized score metadata."""
    not_applicable = semantic_building_rows_not_applicable(score)
    if not_applicable:
        excluded_ids = set(not_applicable["rows"])
        reference = _project_real_query_population(reference, excluded_ids)
        score = _project_real_query_population(score, excluded_ids)
    old = _real_query_score_document(reference)
    new = _real_query_score_document(score)
    old["rows"] = reference.get("rows")
    new["rows"] = score.get("rows")
    old_lines = json.dumps(old, indent=2, sort_keys=True, ensure_ascii=False).splitlines()
    new_lines = json.dumps(new, indent=2, sort_keys=True, ensure_ascii=False).splitlines()
    lines = list(difflib.unified_diff(old_lines, new_lines, fromfile="reference", tofile="head", lineterm=""))
    return ("\n".join(lines) + "\n") if lines else "real_query_behavior:equal\n"


def _first_family_difference(reference: Mapping[str, Any], score: Mapping[str, Any], family: str) -> str | None:
    old_families = reference.get("families", {})
    new_families = score.get("families", {})
    old_family = old_families.get(family) if isinstance(old_families, Mapping) else None
    new_family = new_families.get(family) if isinstance(new_families, Mapping) else None
    if canonical_json(old_family) != canonical_json(new_family):
        return f"{family}.family"
    old_fixture_groups = reference.get("fixture_groups", {})
    new_fixture_groups = score.get("fixture_groups", {})
    old_groups = old_fixture_groups.get(family, {}) if isinstance(old_fixture_groups, Mapping) else {}
    new_groups = new_fixture_groups.get(family, {}) if isinstance(new_fixture_groups, Mapping) else {}
    if not isinstance(old_groups, Mapping) or not isinstance(new_groups, Mapping):
        return f"{family}.fixture_groups"
    for group in sorted(set(old_groups) | set(new_groups)):
        if group not in old_groups or group not in new_groups or canonical_json(old_groups[group]) != canonical_json(new_groups[group]):
            return f"{family}.{group}"
    return None


def _presentation_row_difference(reference: Mapping[str, Any], score: Mapping[str, Any], presentation_rows: Mapping[str, Any]) -> str | None:
    """Compare all row fields, exempting only observed, declared line transitions.

    Validate every declaration even when the scores are equal: a stale allowance
    must fail rather than silently permit a later, unrelated presentation change.
    Row order and identities remain part of the strict comparison.
    """
    old_rows = reference.get("rows", [])
    new_rows = score.get("rows", [])
    if not isinstance(old_rows, list) or not isinstance(new_rows, list):
        return "real_query.rows"
    for episode_id, transition in presentation_rows.items():
        old = next((row for row in old_rows if isinstance(row, Mapping) and row.get("episode_id") == episode_id), None)
        new = next((row for row in new_rows if isinstance(row, Mapping) and row.get("episode_id") == episode_id), None)
        field = f"real_query.{episode_id}.summary_text"
        if old is None:
            return f"{field}:missing_reference_row"
        if new is None:
            return f"{field}:missing_score_row"
        if "summary_text" not in old or canonical_json(old["summary_text"]) != canonical_json(transition["before"]):
            return f"{field}:before_mismatch"
        if "summary_text" not in new or canonical_json(new["summary_text"]) != canonical_json(transition["after"]):
            return f"{field}:after_mismatch"
        if canonical_json(old["summary_text"]) == canonical_json(new["summary_text"]):
            return f"{field}:unchanged"
    for index in range(max(len(old_rows), len(new_rows))):
        old = old_rows[index] if index < len(old_rows) else None
        new = new_rows[index] if index < len(new_rows) else None
        if not isinstance(old, Mapping) or not isinstance(new, Mapping):
            if canonical_json(old) != canonical_json(new):
                return f"real_query.rows[{index}]"
            continue
        episode_id = old.get("episode_id")
        for key in sorted(set(old) | set(new)):
            if key == "summary_text" and episode_id in presentation_rows and episode_id == new.get("episode_id"):
                continue
            if key not in old or key not in new or canonical_json(old[key]) != canonical_json(new[key]):
                return f"real_query.{episode_id or index}.{key}"
    return None


def _engine_unwired_difference(reference: Mapping[str, Any], score: Mapping[str, Any], diff_paths: Sequence[str], presentation_rows: Mapping[str, Any] | None = None) -> str | None:
    row = _first_row_difference(reference, score) if presentation_rows is None else _presentation_row_difference(reference, score, presentation_rows)
    if row:
        return row
    old_score = _real_query_score_document(reference)
    new_score = _real_query_score_document(score)
    difference = _first_value_difference(old_score, new_score)
    if difference:
        return f"real_query.{difference}"
    for family in ("exact_recall", "concept_recall"):
        row = _first_family_difference(reference, score, family)
        if row:
            return row
    for path in diff_paths:
        if path == TOOL_CALL_PARITY_FIXTURE_SOURCE or path.startswith(TOOL_CALL_PARITY_FIXTURE_PREFIX):
            return f"tool_call_parity.{path}"
    return None


SPLIT_ROWS_NOT_APPLICABLE_REASON = (
    "engine ignores the pattern parameter (pattern_probe ignored_pattern): "
    "its split rows were replayed joined into one query, which measures the "
    "single-query surface, not query/pattern fusion"
)

SEMANTIC_BUILDING_NOT_APPLICABLE_FIELD = "semantic_building_rows_not_applicable"


def semantic_building_rows_not_applicable(score: Mapping[str, Any]) -> dict[str, Any] | None:
    placeholders = [
        row
        for row in score.get("rows", [])
        if isinstance(row, Mapping) and "not_applicable_reason" in row
    ]
    declared = score.get(SEMANTIC_BUILDING_NOT_APPLICABLE_FIELD)
    if not placeholders:
        if declared is not None:
            raise InputFault("semantic_building_not_applicable_mismatch:no_placeholder_rows")
        return None
    reasons = {str(row.get("not_applicable_reason", "")).strip() for row in placeholders}
    if "" in reasons or len(reasons) != 1:
        raise InputFault("semantic_building_not_applicable_mismatch:reason")
    rows = sorted(
        (str(row.get("episode_id", "")) for row in placeholders),
        key=episode_number,
    )
    summary = {"reason": next(iter(reasons)), "rows": rows}
    if declared != summary:
        raise InputFault("semantic_building_not_applicable_mismatch:summary")
    return summary


def _project_real_query_population(
    document: Mapping[str, Any], excluded_ids: set[str]
) -> dict[str, Any]:
    projected = copy.deepcopy(dict(document))
    rows = [
        row
        for row in document.get("rows", [])
        if str(row.get("episode_id", "")) not in excluded_ids
    ]
    aggregate = aggregate_real_query(rows)
    projected["rows"] = rows
    projected["families"]["real_query"] = aggregate["family"]
    projected["shapes"] = aggregate["shapes"]
    projected["mechanisms"] = aggregate["mechanisms"]
    projected["census_weighted_mrr_report_only"] = aggregate[
        "census_weighted_mrr_report_only"
    ]
    paired = [row for row in rows if "input_form" in row]
    fixture_groups = projected.get("fixture_groups")
    if isinstance(fixture_groups, dict):
        if paired:
            fixture_groups["real_query"] = {
                "split": mean_metrics([row["metrics"] for row in paired])
            }
        else:
            fixture_groups.pop("real_query", None)
    projected["paired_deltas"] = {
        row["episode_id"]: row["paired_mrr_delta"] for row in paired
    }
    projected.pop(SEMANTIC_BUILDING_NOT_APPLICABLE_FIELD, None)
    split_not_applicable = split_rows_not_applicable(projected)
    if split_not_applicable:
        projected["split_rows_not_applicable"] = split_not_applicable
    else:
        projected.pop("split_rows_not_applicable", None)
    return projected


def project_semantic_building_rows_not_applicable(
    reference: Mapping[str, Any],
    score: Mapping[str, Any],
    manifest: Mapping[str, Any],
) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any], dict[str, Any] | None]:
    not_applicable = semantic_building_rows_not_applicable(score)
    if not not_applicable:
        return dict(reference), dict(score), dict(manifest), None
    validate_scored_population(manifest, score)
    excluded_ids = set(not_applicable["rows"])
    reference_ids = {
        str(row.get("episode_id", ""))
        for row in reference.get("rows", [])
        if isinstance(row, Mapping)
    }
    if not excluded_ids <= reference_ids:
        raise InputFault(
            f"semantic_building_reference_rows_missing:{sorted(excluded_ids - reference_ids)}"
        )
    manifest_rows = [
        row for row in manifest.get("rows", []) if str(row.get("episode_id", "")) not in excluded_ids
    ]
    if not manifest_rows:
        raise InputFault("semantic_building_not_applicable_empty_population")
    projected_manifest = dict(manifest)
    projected_manifest["rows"] = manifest_rows
    return (
        _project_real_query_population(reference, excluded_ids),
        _project_real_query_population(score, excluded_ids),
        projected_manifest,
        not_applicable,
    )


def split_rows_not_applicable(score: Mapping[str, Any]) -> dict[str, Any] | None:
    """Name the split rows the split predicates cannot judge on this engine.

    The replay sends a split row's query and pattern separately only when the
    capability probe shows the engine honours `pattern`; otherwise it joins
    them into one query. A joined row says nothing about split fusion, so
    holding it to the split predicates would refuse every ranking change on
    an engine that has no split support yet. Such rows are recorded here as
    not applicable, with the reason, and skipped. Only an explicit
    `ignored_pattern` probe result skips them: an engine that honours
    `pattern`, or a score without a probe result, is judged in full.
    """
    capability = score.get("capability", {})
    if (
        not isinstance(capability, Mapping)
        or capability.get("pattern_probe") != "ignored_pattern"
        or capability.get("pattern_declared") is not False
    ):
        return None
    rows = sorted(
        (
            str(row.get("episode_id"))
            for row in score.get("rows", [])
            if isinstance(row, Mapping) and row.get("input_form") == "joined"
        ),
        key=episode_number,
    )
    if not rows:
        return None
    return {"reason": SPLIT_ROWS_NOT_APPLICABLE_REASON, "rows": rows}


UNREACHABLE_SPLIT_ROWS_FIELD = "unreachable_split_rows"
# The absolute split predicates a descriptor may waive for a row no pattern
# design can satisfy, with the split kind each one judges. split_paired_harm
# and every non-split predicate are never waivable.
WAIVABLE_SPLIT_PREDICATES = {
    "split_rank1_required": "R1",
    "split_hit5_required": "R3",
    "split_hit3_required": "R4",
    "split_partial_hit_required": "R6",
}
EVIDENCE_TREE = Path(__file__).resolve().parent / ".bench" / "repos" / f"aft-evidence-{EVIDENCE_SHA}"


def validate_unreachable_split_rows(descriptor: Mapping[str, Any]) -> None:
    """Refuse a malformed `unreachable_split_rows` entry before anything runs.

    Each entry is exactly {episode_id, predicate, reason}: a valid episode id,
    one of the absolute split predicates in WAIVABLE_SPLIT_PREDICATES, and a
    non-empty reason. Only a ranking descriptor runs the split predicates, so
    only a ranking descriptor may carry the field.
    """
    if UNREACHABLE_SPLIT_ROWS_FIELD not in descriptor:
        return
    field = UNREACHABLE_SPLIT_ROWS_FIELD
    if descriptor.get("slice_class") != "ranking":
        raise InputFault(f"malformed_descriptor:{field}:ranking_only")
    entries = descriptor[field]
    if not isinstance(entries, list) or not entries:
        raise InputFault(f"malformed_descriptor:{field}:not_a_nonempty_list")
    seen = set()
    for index, entry in enumerate(entries):
        if not isinstance(entry, Mapping) or set(entry) != {"episode_id", "predicate", "reason"}:
            raise InputFault(f"malformed_descriptor:{field}[{index}]:keys_must_be_episode_id_predicate_reason")
        episode_id, predicate, reason = entry["episode_id"], entry["predicate"], entry["reason"]
        if not isinstance(episode_id, str):
            raise InputFault(f"malformed_descriptor:{field}[{index}]:episode_id")
        episode_number(episode_id)
        if predicate not in WAIVABLE_SPLIT_PREDICATES:
            raise InputFault(f"malformed_descriptor:{field}[{index}]:predicate_not_waivable:{predicate}")
        if not isinstance(reason, str) or not reason.strip():
            raise InputFault(f"malformed_descriptor:{field}[{index}]:reason")
        if (episode_id, predicate) in seen:
            raise InputFault(f"malformed_descriptor:{field}[{index}]:duplicate:{episode_id}:{predicate}")
        seen.add((episode_id, predicate))


def read_pinned_answer(path: str) -> str | None:
    """The gold answer file's text at the evidence pin, or None if absent."""
    target = EVIDENCE_TREE / path
    try:
        return target.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return None


def _rank_within_ten(paths: Sequence[Any], answer: str) -> int | None:
    collapsed = collapse_paths(paths)
    return collapsed.index(answer) + 1 if answer in collapsed else None


def _pattern_line_in(pattern: str, text: str) -> tuple[int | None, str | None]:
    """The first 1-based line of `text` the pattern matches, or a reason the
    check cannot be made. The pattern is grep syntax; one Python cannot
    compile is not checkable, so it is never waived."""
    try:
        compiled = re.compile(pattern)
    except re.error as error:
        return None, f"pattern_not_checkable:{error}"
    for number, line in enumerate(text.splitlines(), start=1):
        if compiled.search(line):
            return number, None
    return None, None


def apply_unreachable_split_rows(
    failures: Sequence[str],
    score: Mapping[str, Any],
    manifest: Mapping[str, Any],
    descriptor: Mapping[str, Any],
    read_answer: Callable[[str], str | None] = read_pinned_answer,
) -> tuple[list[str], list[str]]:
    """Waive an absolute split failure for a row no pattern design can satisfy.

    A waiver is honoured only when the run's own data show that (a) the
    row's split rank equals its prose-only rank, so the pattern did not move
    the answer, and (b) the row's pattern matches no line of the gold answer
    file at the evidence pin, so no pattern design could lift it. Otherwise
    the failure stands and a refusal line says which check failed. Returns
    the remaining failures and one line per waiver to print.
    """
    entries = descriptor.get(UNREACHABLE_SPLIT_ROWS_FIELD) or []
    if not entries:
        return list(failures), []
    manifest_rows = {row.get("episode_id"): row for row in manifest.get("rows", [])}
    score_rows = {row.get("episode_id"): row for row in score.get("rows", [])}
    remaining = list(failures)
    notes: list[str] = []
    for entry in entries:
        episode_id, predicate, reason = entry["episode_id"], entry["predicate"], entry["reason"]
        manifest_row = manifest_rows.get(episode_id)
        row = score_rows.get(episode_id)
        if manifest_row is None or row is None or "pattern" not in manifest_row:
            raise InputFault(f"malformed_descriptor:{UNREACHABLE_SPLIT_ROWS_FIELD}:not_a_split_row:{episode_id}")
        if manifest_row.get("split_kind") != WAIVABLE_SPLIT_PREDICATES[predicate]:
            raise InputFault(
                f"malformed_descriptor:{UNREACHABLE_SPLIT_ROWS_FIELD}:predicate_does_not_judge_row:"
                f"{episode_id}:{predicate}:split_kind={manifest_row.get('split_kind')}"
            )
        failure = f"{predicate}:{episode_id}"
        if failure not in remaining:
            notes.append(f"unreachable_split_row_unused:{episode_id}:{predicate}:the row passes")
            continue
        answer = manifest_row["opened_file"]
        split_rank = _rank_within_ten(row.get("ranked_paths", []), answer)
        prose_rank = _rank_within_ten(row.get("prose_only", {}).get("ranked_paths", []), answer)
        ranks = f"split_rank={split_rank or '-'}:prose_rank={prose_rank or '-'}"
        text = read_answer(answer)
        if text is None:
            line, problem = None, f"answer_file_unreadable_at_pin:{answer}"
        else:
            line, problem = _pattern_line_in(str(manifest_row["pattern"]), text)
        refusal = None
        if row.get("input_form") != "split":
            refusal = "not_replayed_in_split_form"
        elif split_rank != prose_rank:
            refusal = f"ranks_differ:{ranks}"
        elif problem is not None:
            refusal = problem
        elif line is not None:
            refusal = f"pattern_matches_answer_file:{answer}:{line}"
        if refusal is not None:
            remaining.append(f"unreachable_split_row_refused:{episode_id}:{predicate}:{refusal}")
            continue
        remaining.remove(failure)
        notes.append(
            f"unreachable_split_row_waived:{episode_id}:{predicate}:{ranks}:"
            f"pattern_in_answer_file=none:reason={reason}"
        )
    return remaining, notes


def split_paired_failures(score: Mapping[str, Any]) -> list[str]:
    """Do not let aggregate gains hide harm to an individual concept answer."""
    failures = []
    not_applicable = split_rows_not_applicable(score)
    skipped = set(not_applicable["rows"]) if not_applicable else set()
    for row in score.get("rows", []):
        if "input_form" not in row:
            continue
        if row["input_form"] == "joined" and row["episode_id"] in skipped:
            continue
        if row["input_form"] != "split":
            raise InputFault(f"split_candidate_required:{row['episode_id']}:joined")
        prose = _metric_block(row.get("prose_only", {}).get("metrics"), f"prose_only:{row['episode_id']}")
        combined = _metric_block(row.get("metrics"), f"split:{row['episode_id']}")
        if row.get("answer_kind") == "concept" and combined["mrr_at_10"] < prose["mrr_at_10"]:
            failures.append(f"split_paired_harm:{row['episode_id']}:mrr_delta={combined['mrr_at_10'] - prose['mrr_at_10']:.6f}")
        kind = row.get("split_kind")
        if kind == "R1" and combined["hit_at_1"] != 1.0:
            failures.append(f"split_rank1_required:{row['episode_id']}")
        if kind == "R3" and combined["hit_at_5"] != 1.0:
            failures.append(f"split_hit5_required:{row['episode_id']}")
        if kind == "R4" and combined["mrr_at_10"] < 1.0 / 3:
            failures.append(f"split_hit3_required:{row['episode_id']}")
        if kind == "R6" and (row.get("envelope_complete") is not False or combined["mrr_at_10"] == 0):
            failures.append(f"split_partial_hit_required:{row['episode_id']}")
    return failures


def evaluate_predicate(reference: Mapping[str, Any], score: Mapping[str, Any], descriptor: Mapping[str, Any], *, missing_ranking_descriptor: bool = False) -> list[str]:
    failures: list[str] = []
    for family in ("exact_recall", "concept_recall"):
        old_family = _metric_block(reference.get("families", {}).get(family), f"reference.{family}")
        new_family = _metric_block(score.get("families", {}).get(family), f"score.{family}")
        for metric in METRICS:
            if new_family[metric] < old_family[metric]:
                failures.append(f"{family}.{metric} below reference")
        old_groups = reference.get("fixture_groups", {}).get(family, {})
        new_groups = score.get("fixture_groups", {}).get(family, {})
        if set(old_groups) != set(new_groups):
            raise InputFault(f"missing_metric_block:{family}.fixture_groups")
        for group in sorted(old_groups):
            old = _metric_block(old_groups[group], f"reference.{family}.{group}")
            new = _metric_block(new_groups[group], f"score.{family}.{group}")
            for metric in METRICS:
                if new[metric] < old[metric]:
                    failures.append(f"{family}.{group}.{metric} below reference")
    old_real = _metric_block(reference.get("families", {}).get("real_query"), "reference.real_query")
    new_real = _metric_block(score.get("families", {}).get("real_query"), "score.real_query")
    if new_real["mrr_at_10"] < old_real["mrr_at_10"]:
        failures.append("real_query.mrr_at_10 below reference")
    old_shapes = reference.get("shapes", {}); new_shapes = score.get("shapes", {})
    if set(old_shapes) != set(new_shapes):
        raise InputFault("missing_metric_block:shapes")
    for shape in sorted(old_shapes):
        old = _metric_block(old_shapes[shape], f"reference.shape.{shape}")
        new = _metric_block(new_shapes[shape], f"score.shape.{shape}")
        if new["mrr_at_10"] + 0.01 < old["mrr_at_10"]:
            failures.append(f"shape {shape} mrr_at_10 loss exceeds 0.01")
        if new["hit_at_5"] < old["hit_at_5"]:
            failures.append(f"shape {shape} hit_at_5 below reference")
    if descriptor["slice_class"] == "ranking" and not missing_ranking_descriptor:
        failures.extend(split_paired_failures(score))
        target = descriptor["targeted_mechanism"]
        if target != "none":
            old = _metric_block(reference.get("mechanisms", {}).get(target), f"reference.mechanism.{target}")
            new = _metric_block(score.get("mechanisms", {}).get(target), f"score.mechanism.{target}")
            if new["mrr_at_10"] <= old["mrr_at_10"]:
                failures.append(f"targeted mechanism {target} did not improve")
            if new["hit_at_5"] < old["hit_at_5"]:
                failures.append(f"targeted mechanism {target} hit_at_5 below reference")
    elif descriptor["slice_class"] == "non_ranking":
        fixture_results = score.get("fixture_results", {})
        for fixture in descriptor["fixtures"]:
            if fixture_results.get(fixture) is not True:
                failures.append(f"binary fixture failed:{fixture}")
    if missing_ranking_descriptor:
        failures.append("missing ranking descriptor")
    return failures


@dataclass(frozen=True)
class GateResult:
    exit_code: int
    reasons: tuple[str, ...]
    # Diagnostics printed on success too, including classification decisions.
    notes: tuple[str, ...] = ()


def validate_candidate_page_invariance(
    reference: Mapping[str, Any], score: Mapping[str, Any], slice_class: str
) -> None:
    """Check evaluated rows that failed page invariance against the slice class.

    A ranking slice changes the engine, so every row must be page-invariant,
    whatever REFERENCE_NOT_PAGE_INVARIANT_FIELD says: the field excuses only
    the reference. Any other slice leaves the engine alone, so a row must
    reproduce the reference exactly: a recorded miss must fail invariance
    with the same record and zero metrics again, and a row the reference
    scored normally must not fail.
    """
    recorded = {
        row.get("episode_id"): row
        for row in reference.get("rows", [])
        if isinstance(row, Mapping)
    }
    for row in score.get("rows", []):
        if not isinstance(row, Mapping):
            continue
        failure = row.get(PAGE_INVARIANCE_FAILED_FIELD)
        if slice_class == "ranking":
            if failure:
                raise InputFault(f"{failure}:candidate")
            continue
        reference_row = recorded.get(row.get("episode_id"), {})
        reference_failure = reference_row.get(PAGE_INVARIANCE_FAILED_FIELD)
        if not failure and not reference_failure:
            continue
        misses = all(
            float(candidate.get("metrics", {}).get(metric, 1.0)) == 0.0
            for candidate in (row, reference_row)
            for metric in METRICS
        )
        if failure != reference_failure or not misses:
            raise InputFault(f"page_invariance_failed:{row.get('episode_id')}:reference_miss_not_reproduced")


def total_gate(reference: Mapping[str, Any], score: Mapping[str, Any], manifest: Mapping[str, Any], descriptor: Mapping[str, Any] | None, diff_paths: Sequence[str], read_answer: Callable[[str], str | None] = read_pinned_answer, *, schema_blobs: tuple[bytes | None, bytes | None] | None = None) -> GateResult:
    notes: list[str] = []
    try:
        resolved, missing = resolve_descriptor(descriptor, diff_paths, schema_blobs=schema_blobs, notes=notes)
        not_applicable = semantic_building_rows_not_applicable(score)
        if not_applicable:
            skip_summary = not_applicable
            reference, score, manifest, _ = project_semantic_building_rows_not_applicable(
                reference, score, manifest
            )
            notes.append(
                f"semantic_building_rows_skipped:{','.join(skip_summary['rows'])}:{skip_summary['reason']}"
            )
        validate_candidate_page_invariance(reference, score, str(resolved["slice_class"]))
        if resolved["slice_class"] == "engine_unwired":
            difference = _engine_unwired_difference(reference, score, diff_paths, resolved.get("presentation_rows"))
            if difference:
                raise InputFault(f"engine_unwired_mismatch:row={difference}")
        if resolved["slice_class"] == "ranking" and resolved["targeted_mechanism"] == "none":
            difference = _engine_unwired_difference(reference, score, diff_paths)
            if difference:
                raise InputFault(f"latency_only_ranking_mismatch:row={difference}")
        validate_scored_population(manifest, score)
        validate_profile_score(score)
        reasons = evaluate_predicate(reference, score, resolved, missing_ranking_descriptor=missing)
        if resolved["slice_class"] == "ranking" and not missing:
            reasons, waiver_notes = apply_unreachable_split_rows(reasons, score, manifest, resolved, read_answer)
            notes.extend(waiver_notes)
    except InputFault as error:
        return GateResult(2, (str(error),), tuple(notes))
    return GateResult(1 if reasons else 0, tuple(reasons), tuple(notes))


def quantize6(value: float) -> str:
    return format(Decimal.from_float(value).quantize(Decimal("0.000001"), rounding=ROUND_HALF_EVEN), "f")


def wilson(point: float, size: float, z: float = 1.959964) -> tuple[float, float]:
    if size <= 0 or not math.isfinite(size):
        raise InputFault("invalid_effective_size")
    denominator = 1.0 + z * z / size
    center = (point + z * z / (2.0 * size)) / denominator
    radius = z * math.sqrt((point * (1.0 - point) / size) + z * z / (4.0 * size * size)) / denominator
    return max(0.0, center - radius), min(1.0, center + radius)


def estimator(rows: Sequence[Mapping[str, Any]], population: Mapping[str, Any], *, discriminating: int, all_episodes: int) -> dict[str, Any]:
    labelled_total = sum(int(row.get("n_s", 0)) for row in rows)
    if labelled_total == 0:
        return {"labelled_total": 0, "population": dict(population), "estimator": "undefined_empty_population"}
    by_name = {str(row["census_stratum"]): row for row in rows}
    if set(by_name) != set(STRATA):
        raise InputFault("estimator_strata_mismatch")
    sampled = [name for name in STRATA if int(by_name[name]["n_s"]) > 0]
    mass = sum(STRATUM_SHARES[name] for name in sampled)
    table: list[dict[str, Any]] = []
    weighted = 0.0; size_denominator = 0.0; failures = 0
    for name in STRATA:
        n_s = int(by_name[name]["n_s"]); f_s = int(by_name[name]["f_s"])
        if n_s < 0 or not 0 <= f_s <= n_s:
            raise InputFault(f"invalid_estimator_row:{name}")
        failures += f_s
        output = {"census_stratum": name, "c_s": STRATUM_SHARES[name], "n_s": n_s, "f_s": f_s, "sampled": n_s > 0}
        if n_s:
            p_s = f_s / n_s; w_s = STRATUM_SHARES[name] / mass
            output.update({"p_s": float(quantize6(p_s)), "w_s": float(quantize6(w_s))})
            weighted += w_s * p_s; size_denominator += w_s * w_s / n_s
        else:
            output.update({"w_s": 0, "reason": "no_labelled_episodes"})
        table.append(output)
    effective = 1.0 / size_denominator
    weighted_interval = wilson(weighted, effective)
    unweighted = failures / labelled_total
    unweighted_interval = wilson(unweighted, labelled_total)
    if all_episodes <= 0 or not 0 <= discriminating <= all_episodes:
        raise InputFault("invalid_measured_window")
    q = discriminating / all_episodes
    projected = weighted * q
    # Assume sampled labels are independent of the measured fraction of discriminating episodes.
    # Covariance is therefore zero; that fraction uses binomial variance and the weighted rate uses its effective sample size.
    var_p = weighted * (1 - weighted) / effective
    var_q = q * (1 - q) / all_episodes
    var_product = q * q * var_p + weighted * weighted * var_q
    radius = 1.959964 * math.sqrt(max(0.0, var_product))
    projected_interval = (max(0.0, projected - radius), min(1.0, projected + radius))
    return {
        "labelled_total": labelled_total,
        "population": dict(population),
        "estimator": "stratified_conditional_wilson_delta_v1",
        "rows": table,
        "dropped_census_mass_remeasure": float(quantize6(1.0 - mass)),
        "weighted_conditional": {"point": float(quantize6(weighted)), "wilson95": [float(quantize6(x)) for x in weighted_interval]},
        "unweighted_conditional": {"point": float(quantize6(unweighted)), "wilson95": [float(quantize6(x)) for x in unweighted_interval]},
        "effective_size_unrounded": effective,
        "effective_size": float(quantize6(effective)),
        "measured_window": {"D": discriminating, "N": all_episodes, "share": float(quantize6(q))},
        "projected_all_episode": {"point": float(quantize6(projected)), "delta95": [float(quantize6(x)) for x in projected_interval]},
        "variance_assumptions": {
            "label_outcomes": "independent Bernoulli within the effective-size approximation",
            "measured_D_over_N": "independent binomial proportion",
            "covariance": 0,
            "formula": "q^2 Var(p_w) + p_w^2 Var(q)",
        },
        "comparisons": {
            "conditional": {"prior_numerator": 3996, "prior_denominator": 6469, "prior_point": float(quantize6(3996 / 6469))},
            "projected": {"prior_numerator": 3996, "prior_denominator": 20183, "prior_point": float(quantize6(3996 / 20183))},
        },
    }


def atomic_write_pair(reference_path: Path, sidecar_path: Path, reference: bytes, sidecar: bytes, *, fault: str | None = None) -> None:
    """Write a validated pair and restore the exact old pair after any fault."""
    old = {reference_path: reference_path.read_bytes() if reference_path.exists() else None,
           sidecar_path: sidecar_path.read_bytes() if sidecar_path.exists() else None}
    reference_path.parent.mkdir(parents=True, exist_ok=True)
    temporary: dict[Path, Path] = {}
    try:
        for stage, path, data in (("reference", reference_path, reference), ("sidecar", sidecar_path, sidecar)):
            if fault == f"write_{stage}": raise OSError("injected")
            fd, name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
            temporary[path] = Path(name)
            with os.fdopen(fd, "wb") as handle:
                handle.write(data)
                if fault == f"fsync_{stage}": raise OSError("injected")
                handle.flush(); os.fsync(handle.fileno())
        json.loads(reference)
        if fault == "validation": raise ValueError("injected")
        if fault == "rename_reference": raise OSError("injected")
        os.replace(temporary[reference_path], reference_path); temporary.pop(reference_path)
        if fault == "between_renames": raise OSError("injected")
        if fault == "rename_sidecar": raise OSError("injected")
        os.replace(temporary[sidecar_path], sidecar_path); temporary.pop(sidecar_path)
        directory_fd = os.open(reference_path.parent, os.O_RDONLY)
        try: os.fsync(directory_fd)
        finally: os.close(directory_fd)
    except Exception as error:
        for path, data in old.items():
            if data is None:
                path.unlink(missing_ok=True)
            else:
                restore = path.with_name(f".{path.name}.restore")
                restore.write_bytes(data); os.replace(restore, path)
        raise InputFault(f"reference_transaction_failure:{fault or type(error).__name__}") from error
    finally:
        for path in temporary.values():
            path.unlink(missing_ok=True)


def identity_delta(old: Mapping[str, Any], new: Mapping[str, Any]) -> dict[str, list[str]]:
    old_rows = {row["episode_id"]: row for row in old.get("rows", [])}
    new_rows = {row["episode_id"]: row for row in new.get("rows", [])}
    return {
        "added": sorted(set(new_rows) - set(old_rows)),
        "removed": sorted(set(old_rows) - set(new_rows)),
        "changed": sorted(key for key in set(old_rows) & set(new_rows) if old_rows[key] != new_rows[key]),
    }


def ensure_loopback(host: str) -> None:
    try:
        addresses = {item[4][0] for item in socket.getaddrinfo(host, None)}
    except socket.gaierror as error:
        raise InputFault(f"non_loopback_network:{host}") from error
    if not addresses or any(address not in {"127.0.0.1", "::1"} for address in addresses):
        raise InputFault(f"non_loopback_network:{host}")
