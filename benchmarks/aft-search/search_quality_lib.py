#!/usr/bin/env python3
"""Deterministic, offline primitives for the aft_search quality benchmark.

This module intentionally uses only the Python standard library.  CI can therefore
validate benchmark inputs without installing packages or consulting a network.
"""

from __future__ import annotations

import hashlib
import json
import math
import os
import re
import shutil
import socket
import struct
import tempfile
from collections import Counter, defaultdict
from dataclasses import dataclass
from decimal import Decimal, ROUND_HALF_EVEN
from pathlib import Path
from typing import Any, Iterable, Iterator, Mapping, Sequence

EVIDENCE_SHA = "30d4a64f99b3b15fd88be6cf962fff4b3fe5ea17"
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
    "packages/pi-plugin/",
    "packages/opencode-plugin/",
    "benchmarks/aft-search/engine-fixtures/",
)

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
    return {metric: sum(float(row[metric]) for row in rows) / len(rows) for metric in METRICS}


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
    weighted_mrr = sum(STRATUM_SHARES[name] * mean_metrics(values)["mrr_at_10"] for name, values in by_stratum.items())
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
        return [{"topK": 100}]
    if profile == "paged":
        if not offset_declared:
            raise InputFault("illegal_profile:offset_not_declared")
        return [{"topK": 100, "offset": offset} for offset in (0, 100, 200, 300)]
    raise InputFault(f"illegal_profile:{profile}")


def invariance_requests() -> tuple[list[dict[str, int]], ...]:
    return (
        [{"topK": 10, "offset": offset} for offset in range(0, 100, 10)],
        [{"topK": 25, "offset": offset} for offset in (0, 25, 50, 75)],
        [{"topK": 100, "offset": 0}],
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
            if len(requests) != 1 or requests[0].get("topK") != 100 or "offset" in requests[0] or invariance:
                raise InputFault(f"request_bound_violation:{row.get('episode_id')}:single_page")
        else:
            offsets = [request.get("offset") for request in requests]
            if not offset_declared or len(requests) != 4 or offsets != [0, 100, 200, 300] or any(request.get("topK") != 100 for request in requests):
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
            for request in all_requests
        ):
            raise InputFault(f"replay_input_mismatch:{row.get('episode_id')}:request_set")
        if not isinstance(row.get("retrieval_depth"), int) or row["retrieval_depth"] < 0:
            raise InputFault(f"invalid_stop_fields:{row.get('episode_id')}:retrieval_depth")
        ranked_paths = row.get("ranked_paths")
        if not isinstance(ranked_paths, list) or len(ranked_paths) > 10 or len(ranked_paths) != len(set(ranked_paths)):
            raise InputFault(f"invalid_stop_fields:{row.get('episode_id')}:ranked_paths")


def included_manifest_ids(manifest: Mapping[str, Any]) -> list[str]:
    rows = manifest.get("rows")
    if not isinstance(rows, list):
        raise InputFault("malformed_schema:manifest_rows")
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
    for expected_id, row in zip(sorted(expected, key=episode_number), sorted(score_rows, key=lambda item: episode_number(str(item["episode_id"])))):
        if expected_id != row["episode_id"]:
            raise InputFault("scored_population_mismatch")
        manifest_row = next(item for item in manifest["rows"] if item["episode_id"] == expected_id)
        sent = row.get("request", {}).get("includeTests")
        if sent is not manifest_row.get("include_tests") or row.get("include_tests_source") != manifest_row.get("include_tests_source"):
            raise InputFault(
                f"replay_input_mismatch:{expected_id}:expected={manifest_row.get('include_tests')}/"
                f"{manifest_row.get('include_tests_source')}:sent={sent}"
            )
        stop = row.get("collapse_stop_reason")
        if stop not in STOP_REASONS:
            raise InputFault(f"invalid_stop_fields:{expected_id}:{stop}")
        if not isinstance(row.get("pages_fetched"), int) or row["pages_fetched"] < 1:
            raise InputFault(f"invalid_stop_fields:{expected_id}:pages_fetched")


def derive_slice_class(paths: Sequence[str]) -> str:
    if not isinstance(paths, list) or any(not isinstance(path, str) or path.startswith("/") or ".." in Path(path).parts for path in paths):
        raise InputFault("unresolvable_diff")
    return "ranking" if any(any(path == prefix or path.startswith(prefix) for prefix in RANKING_FENCE_PREFIXES) for path in paths) else "non_ranking"


def resolve_descriptor(descriptor: Mapping[str, Any] | None, diff_paths: Sequence[str]) -> tuple[dict[str, Any], bool]:
    derived = derive_slice_class(list(diff_paths))
    missing_ranking = descriptor is None and derived == "ranking"
    if descriptor is None:
        return {"slice_class": derived, "targeted_mechanism": "none", "kind": "harness", "fixtures": ["harness-goldens"]}, missing_ranking
    declared = descriptor.get("slice_class")
    if declared != derived:
        raise InputFault(f"descriptor_class_mismatch:declared={declared}:derived={derived}")
    target = descriptor.get("targeted_mechanism")
    if derived == "ranking":
        if target not in MECHANISMS:
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
        target = descriptor["targeted_mechanism"]
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


def total_gate(reference: Mapping[str, Any], score: Mapping[str, Any], manifest: Mapping[str, Any], descriptor: Mapping[str, Any] | None, diff_paths: Sequence[str]) -> GateResult:
    try:
        validate_scored_population(manifest, score)
        validate_profile_score(score)
        resolved, missing = resolve_descriptor(descriptor, diff_paths)
        reasons = evaluate_predicate(reference, score, resolved, missing_ranking_descriptor=missing)
    except InputFault as error:
        return GateResult(2, (str(error),))
    return GateResult(1 if reasons else 0, tuple(reasons))


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
