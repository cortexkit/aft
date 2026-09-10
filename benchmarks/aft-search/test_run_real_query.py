#!/usr/bin/env python3
from __future__ import annotations

import copy
import json
import tempfile
import unittest
from pathlib import Path
from typing import Any, Mapping

from run_exact_recall import CorpusMissing, validate_corpus
from run_real_query import assemble_score, score_manifest_rows
from search_quality_lib import InputFault, canonical_json, choose_stop, validate_profile_score
from setup_corpus import parse_corpus_toml


class FakeClient:
    def __init__(self, total: int = 3):
        self.total = total
        self.calls: list[dict[str, Any]] = []

    def search(self, arguments: Mapping[str, Any]) -> dict[str, Any]:
        request = dict(arguments)
        self.calls.append(request)
        offset = int(request.get("offset", 0))
        top_k = int(request["topK"])
        if self.total == 3:
            paths = ["tests/recorded_true_test.py", "src/main.py", "src/other.py"] if request.get("includeTests") else ["src/main.py", "src/other.py"]
        else:
            paths = [f"src/file{index:03}.py" for index in range(self.total)]
        selected = paths[offset: offset + top_k]
        return {
            "success": True,
            "status": "ready",
            "results": [{"file": path, "name": Path(path).stem} for path in selected],
            "more_available": offset + top_k < len(paths),
        }


class CrossBoundaryDuplicateClient(FakeClient):
    def __init__(self) -> None:
        super().__init__(total=500)

    def search(self, arguments: Mapping[str, Any]) -> dict[str, Any]:
        request = dict(arguments)
        self.calls.append(request)
        offset = int(request.get("offset", 0))
        top_k = int(request["topK"])
        paths = [f"src/file{index:03}.py" for index in range(self.total)]
        paths[99] = paths[0]
        selected = paths[offset : offset + top_k]
        return {
            "success": True,
            "status": "ready",
            "results": [{"file": path, "name": Path(path).stem} for path in selected],
            "more_available": offset + top_k < len(paths),
        }


def manifest(include_tests: bool = True) -> dict[str, Any]:
    return {
        "rows": [
            {
                "episode_id": "followup-census:1",
                "query": "recorded test visibility",
                "opened_file": "tests/recorded_true_test.py",
                "include_tests": include_tests,
                "include_tests_source": "recorded",
                "pinned_shape": "natural_language",
                "mechanism": "topk_cut",
                "census_stratum": "nl",
            }
        ]
    }


def exact_report() -> dict[str, Any]:
    return {"results": [{"repo": "fixture", "family": "sentence", "rank": 1}]}


def concept_report() -> dict[str, Any]:
    return {
        "rows": [
            {
                "fixture_group": "natural_language",
                "mrr_at_10": 1.0,
                "hit_at_1": 1.0,
                "hit_at_5": 1.0,
            }
        ]
    }


class RealQueryRunnerTests(unittest.TestCase):
    def test_runner_is_byte_deterministic_on_the_same_tree(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest_path = root / "manifest.json"
            binary = root / "aft"
            reference = root / "missing-reference.json"
            document = manifest()
            manifest_path.write_bytes(canonical_json(document))
            binary.write_bytes(b"fake-aft-binary")
            outputs = []
            for _ in range(2):
                capability = {"schema_path": "fixture.json", "schema_sha256": "0" * 64, "offset_declared": False}
                rows = score_manifest_rows(document, "single_page", capability, FakeClient(), root)
                score = assemble_score(
                    document,
                    rows,
                    "single_page",
                    capability,
                    "fixture-model",
                    exact_report(),
                    concept_report(),
                    manifest_path,
                    binary,
                    reference,
                )
                outputs.append(canonical_json(score))
            self.assertEqual(outputs[0], outputs[1])

    def test_single_page_request_grammar_rejects_an_offset(self) -> None:
        capability = {"schema_path": "fixture.json", "schema_sha256": "0" * 64, "offset_declared": False}
        client = FakeClient()
        rows = score_manifest_rows(manifest(), "single_page", capability, client, Path("."))
        self.assertEqual(client.calls, [{"query": "recorded test visibility", "topK": 100, "includeTests": True}])
        score = {"profile": "single_page", "capability": capability, "rows": copy.deepcopy(rows)}
        score["rows"][0]["requests"][0]["offset"] = 0
        with self.assertRaisesRegex(InputFault, "request_bound_violation.*single_page"):
            validate_profile_score(score)

    def test_paged_profile_executes_four_scoring_and_10_4_1_invariance_requests(self) -> None:
        capability = {
            "schema_path": "fixture.json",
            "schema_sha256": "0" * 64,
            "offset_declared": True,
            "offset_bounds": {"minimum": 0, "maximum": 10000},
        }
        client = FakeClient(total=500)
        rows = score_manifest_rows(manifest(), "paged", capability, client, Path("."))
        self.assertEqual(rows[0]["pages_fetched"], 4)
        self.assertEqual([len(plan) for plan in rows[0]["invariance_requests"]], [10, 4, 1])
        self.assertEqual(rows[0]["request_count"], 19)
        self.assertTrue(capability["probe_pages_differ"])
        validate_profile_score({"profile": "paged", "capability": capability, "rows": rows})

    def test_page_zero_paths_preserve_cross_boundary_duplicate_cut(self) -> None:
        capability = {
            "schema_path": "fixture.json",
            "schema_sha256": "0" * 64,
            "offset_declared": True,
            "offset_bounds": {"minimum": 0, "maximum": 10000},
        }
        rows = score_manifest_rows(
            manifest(), "paged", capability, CrossBoundaryDuplicateClient(), Path(".")
        )
        page_zero = rows[0]["page_zero_ranked_paths"]
        self.assertEqual(len(page_zero), 99)
        self.assertEqual(page_zero, [f"src/file{index:03}.py" for index in range(99)])
        self.assertNotIn("src/file099.py", page_zero)

    def test_stop_token_precedence_is_page_cap_then_exhausted_then_ten_files(self) -> None:
        self.assertEqual(choose_stop(page_cap=True, exhausted=True, ten_files=True), "page_cap")
        self.assertEqual(choose_stop(page_cap=False, exhausted=True, ten_files=True), "exhausted")
        self.assertEqual(choose_stop(page_cap=False, exhausted=False, ten_files=True), "ten_files")

    def test_missing_exact_recall_corpus_names_provision_command(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus_path = root / "corpus" / "corpus.toml"
            corpus_path.parent.mkdir()
            corpus_path.write_text(
                '[corpus]\nclone_root = ".bench/repos"\n[[repos]]\nname = "missing"\nurl = "https://example.invalid/missing.git"\ncommit = "0123456789012345678901234567890123456789"\n'
            )
            corpus, repos = parse_corpus_toml(corpus_path)
            with self.assertRaisesRegex(CorpusMissing, r"corpus_missing:missing:run=python3 benchmarks/aft-search/provision_corpus.py"):
                validate_corpus(corpus_path, corpus, repos)

    def test_recorded_include_tests_changes_ranked_paths(self) -> None:
        true_rows = score_manifest_rows(manifest(True), "single_page", {"offset_declared": False}, FakeClient(), Path("."))
        false_rows = score_manifest_rows(manifest(False), "single_page", {"offset_declared": False}, FakeClient(), Path("."))
        self.assertEqual(true_rows[0]["request"]["includeTests"], True)
        self.assertEqual(false_rows[0]["request"]["includeTests"], False)
        self.assertNotEqual(true_rows[0]["ranked_paths"], false_rows[0]["ranked_paths"])
        self.assertEqual(true_rows[0]["ranked_paths"][0], "tests/recorded_true_test.py")


if __name__ == "__main__":
    unittest.main()
