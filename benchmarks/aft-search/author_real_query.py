#!/usr/bin/env python3
"""Author the B1 real-query manifest from read-only census exports.

The command is intentionally separate from CI: census stores and repository
network access are authoring inputs, while verify/evaluate consume only its
checked-in output.
"""

from __future__ import annotations

import argparse
import ast
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

from evidence_tree import evidence_tree_sha256_from_contents, pinned_text_contents
from search_quality_lib import (
    EVIDENCE_SHA, FIXTURE_IDS, MECHANISMS, PINNED_SHAPES, QUERY_KINDS, STRATA,
    InputFault, canonical_json, sample_plan, sha256_file, validate_census,
)

ARTIFACT_NAMES = ("summary.md", "mechanism-report.md", "labels.jsonl", "mechanisms.jsonl")


def load_jsonl(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    with path.open(encoding="utf-8") as handle:
        for number, line in enumerate(handle, 1):
            if line.strip():
                value = json.loads(line)
                if not isinstance(value, dict):
                    raise InputFault(f"invalid_jsonl:{path}:{number}")
                rows.append(value)
    return rows


def canonical_episode(row: dict[str, Any]) -> dict[str, Any]:
    if "episode_number" not in row:
        raise InputFault("missing_episode_number")
    result = dict(row)
    result["episode_id"] = f"followup-census:{int(result.pop('episode_number'))}"
    if "shape" in result and "census_stratum" not in result:
        result["census_stratum"] = result["shape"]
    if "label" in result and "mechanism" not in result:
        result["mechanism"] = result["label"]
    return result


def validate_digests(census_dir: Path, expected_path: Path) -> dict[str, str]:
    expected = json.loads(expected_path.read_text())
    if set(expected) != set(ARTIFACT_NAMES):
        raise InputFault("canonical_census_digest_set")
    actual = {name: sha256_file(census_dir / name) for name in ARTIFACT_NAMES}
    if actual != expected:
        raise InputFault(f"canonical_census_digest_mismatch:expected={expected}:actual={actual}")
    return actual


def pinned_domains(repo: Path) -> dict[str, Any]:
    def at_pin(path: str) -> str:
        command = ["git", "show", f"{EVIDENCE_SHA}:{path}"]
        result = subprocess.run(command, cwd=repo, text=True, capture_output=True, check=False)
        if result.returncode:
            raise InputFault(f"pin_source_missing:{path}")
        return result.stdout

    query = at_pin("crates/aft/src/query_shape.rs")
    plan = at_pin("crates/aft/src/commands/semantic_search/plan_table.rs")
    query_body = re.search(r"pub enum QueryKind\s*\{([^}]+)\}", query, re.S)
    shape_body = re.search(r"pub enum SearchShape\s*\{([^}]+)\}", plan, re.S)
    if not query_body or not shape_body:
        raise InputFault("pinned_enum_parse")
    parse = lambda body: tuple(re.findall(r"^\s*([A-Z][A-Za-z0-9_]*)\s*,", body, re.M))
    if parse(query_body.group(1)) != QUERY_KINDS or parse(shape_body.group(1)) != QUERY_KINDS:
        raise InputFault("pinned_enum_domain_mismatch")
    conversions = dict(zip(QUERY_KINDS, PINNED_SHAPES))
    for variant, serialized in conversions.items():
        if f"SearchShape::{variant} => \"{serialized}\"" not in plan or f"SearchShape::{variant} => QueryKind::{variant}" not in plan:
            raise InputFault(f"pinned_conversion_mismatch:{variant}")
    return {"query_kind": list(QUERY_KINDS), "search_shape": list(QUERY_KINDS), "conversion": conversions}


def projection_function(mechanism_py: Path) -> str:
    tree = ast.parse(mechanism_py.read_text())
    candidates: list[str] = []
    for node in tree.body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            source = ast.get_source_segment(mechanism_py.read_text(), node) or ""
            if '"shape": query_shape(query)' in source and '"episode_number"' in source:
                candidates.append(node.name)
    if candidates != ["mechanism_record"]:
        raise InputFault(f"projection_source_function_resolution:{candidates}")
    return candidates[0]


def pinned_evidence(repo: Path) -> tuple[str, set[str], dict[str, str]]:
    """Project the pin without accepting any query or relevance-label argument."""
    contents = pinned_text_contents(repo, EVIDENCE_SHA)
    return evidence_tree_sha256_from_contents(contents), set(contents), contents


def classify_queries(repo: Path, queries: list[str]) -> list[str]:
    manifest = repo / "benchmarks/aft-search/query-shape-helper/Cargo.toml"
    payload = "".join(json.dumps(query) + "\n" for query in queries)
    result = subprocess.run(["cargo", "run", "--quiet", "--manifest-path", str(manifest)], cwd=repo, input=payload, text=True, capture_output=True, check=False)
    if result.returncode:
        raise InputFault(f"pinned_classifier_failed:{result.stderr.strip()}")
    shapes = result.stdout.splitlines()
    if len(shapes) != len(queries) or any(shape not in PINNED_SHAPES for shape in shapes):
        raise InputFault("pinned_classifier_output")
    return shapes


def vector_pack_binding(output: Path) -> str:
    """Digest of the existing vector pack, or "unbound" before the first capture.

    Authoring never writes vectors. `capture_real_query_vectors.py` embeds what
    AFT actually indexes with the real model and rebinds every row, and the
    gate refuses a row whose binding does not match the pack on disk.
    """
    return sha256_file(output) if output.is_file() else "unbound"


def query_tokens(query: str) -> set[str]:
    return {token.casefold() for token in re.findall(r"[A-Za-z0-9_]+", query) if len(token) >= 3}


def competitor_counts(query: str, opened: str, contents: dict[str, str]) -> int:
    tokens = query_tokens(query)
    return sum(path != opened and all(token in text.casefold() for token in tokens) for path, text in contents.items())


def author(args: argparse.Namespace) -> dict[str, Any]:
    census_dir = Path(args.census_dir)
    digests = validate_digests(census_dir, Path(args.expected_digests))
    mechanisms = [canonical_episode(row) for row in load_jsonl(census_dir / "mechanisms.jsonl")]
    labels = [canonical_episode(row) for row in load_jsonl(census_dir / "labels.jsonl")]
    validate_census(mechanisms, labels)
    domains = pinned_domains(Path(args.repo_root))
    producer = projection_function(census_dir / "mechanism.py")
    repo = Path(args.repo_root)
    tree_digest, evidence_paths, evidence_contents = pinned_evidence(repo)
    label_ids = {row["episode_id"] for row in labels}
    plan = sample_plan(mechanisms, args.manifest_seed, label_ids)
    vector_path = Path(args.vector_output)
    vector_digest = vector_pack_binding(vector_path)
    pinned_by_id = dict(zip((row["episode_id"] for row in mechanisms), classify_queries(repo, [row["query"] for row in mechanisms])))
    source_by_id = {row["episode_id"]: row for row in mechanisms}
    label_by_id = {row["episode_id"]: row for row in labels}
    rows: list[dict[str, Any]] = []
    selected = set(plan["union_order"])
    for episode_id in sorted(selected, key=lambda value: int(value.split(":")[1])):
        source = source_by_id.get(episode_id, {})
        label = label_by_id.get(episode_id)
        known = {key: source[key] for key in ("repo", "sha", "query", "census_stratum", "mechanism") if key in source}
        if label and "mechanism" in label:
            known["mechanism"] = label["mechanism"]
        row: dict[str, Any] = {"episode_id": episode_id, **known, "dual_source": episode_id in plan["dual_source"]}
        if label is None:
            row["excluded_reason"] = "repo_unowned_or_unavailable"
        else:
            cited = [str(path).strip() for path in label.get("files_cited", []) if str(path).strip()]
            if cited:
                row["opened_file"] = cited[0]
            opened = row.get("opened_file", "")
            if opened.startswith("/") or opened not in evidence_paths:
                row["excluded_reason"] = "repo_unowned_or_unavailable"
            else:
                row.update({"repo": "cortexkit/aft", "sha": EVIDENCE_SHA})
            captured = source.get("includeTests", source.get("include_tests", None))
            if captured is None:
                if bool(source.get("test_support_provenance_indeterminate")):
                    row["excluded_reason"] = "include_tests_indeterminate"
                else:
                    row.update({"include_tests": False, "include_tests_source": "default"})
            else:
                row.update({"include_tests": bool(captured), "include_tests_source": "recorded"})
            if "excluded_reason" not in row:
                row.update({
                    "pinned_shape": pinned_by_id[episode_id],
                    "recorded_top_k": int(source.get("top_k", source.get("recorded_top_k", 10))),
                    "confidence_split": "calibration" if int(episode_id.split(":")[1]) % 2 == 0 else "evaluation",
                    "pruning_policy_id": "v1",
                    "evidence_tree_sha256": tree_digest,
                    "embedding_pack": str(vector_path.relative_to(repo)), "embedding_pack_sha256": vector_digest,
                    "competitor_count": competitor_counts(source["query"], row["opened_file"], evidence_contents),
                    "policy_excluded_competitor_count": 0,
                })
                if row["pinned_shape"] not in PINNED_SHAPES:
                    raise InputFault(f"invalid_pinned_shape:{episode_id}:{row['pinned_shape']}")
        rows.append(row)
    output = {
        "schema": "aft-search-real-query-manifest-v1", "evidence_sha": EVIDENCE_SHA,
        "manifest_seed": args.manifest_seed, "census_artifact_sha256": digests,
        "census_unique_identities": 6469, "census_stratum_counts": {name: sum(row["census_stratum"] == name for row in mechanisms) for name in STRATA},
        "retained_labels": 300, "mechanism_projection_sum": 6470,
        "mechanism_projection_is_identity_partition": False,
        "mechanism_projection": {name: {"estimated_episodes": value[0], "estimated_share": value[1], "fixture_episode_ids": [f"followup-census:{number}" for number in value[2]]} for name, value in MECHANISMS.items()},
        "mechanism_projection_producer": {"path": ".alfonso/data/aft-search-followup-census/mechanism.py", "function": producer},
        "confidence_handoff": {"membership": "even suffix calibration; odd suffix evaluation", "owner": "campaign A", "immutable_rule": "precision(high)-precision(low)>=0.15; support>=50 in each class"},
        "shape_drift": {"count": 0, "rows": []},
        "pinned_domains": domains, "sample_plan": plan, "rows": rows,
    }
    Path(args.output).write_bytes(canonical_json(output))
    if args.plan_output:
        Path(args.plan_output).write_bytes(canonical_json(plan))
    return output


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser()
    result.add_argument("--census-dir", required=True)
    result.add_argument("--expected-digests", required=True)
    result.add_argument("--repo-root", default=str(Path(__file__).resolve().parents[2]))
    result.add_argument("--manifest-seed", default="20260908")
    result.add_argument("--vector-output", default=str(Path(__file__).resolve().parent / "real-query-vectors.bin"))
    result.add_argument("--output", required=True)
    result.add_argument("--plan-output")
    return result


def main() -> int:
    try:
        author(parser().parse_args())
    except (InputFault, OSError, json.JSONDecodeError, ValueError) as error:
        print(f"authoring_fault:{error}", file=sys.stderr)
        return 2
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
