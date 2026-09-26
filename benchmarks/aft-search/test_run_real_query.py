#!/usr/bin/env python3
from __future__ import annotations

import copy
import json
import struct
import tempfile
import threading
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Mapping

from embedding_fixture_server import Server, corpus_key, query_key
from evidence_tree import evidence_tree_sha256
from provision_evidence import provision
from run_exact_recall import CorpusMissing, validate_corpus
from run_real_query import ROOT, assemble_score, load_inputs, score_manifest_rows
from search_quality_lib import (
    D_0,
    EVIDENCE_SHA,
    INVARIANCE_DEPTH,
    PAGE_SIZE,
    InputFault,
    canonical_json,
    choose_stop,
    invariance_requests,
    validate_profile_score,
)
from setup_corpus import parse_corpus_toml
from vector_pack import read_pack, write_pack


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
        paths[PAGE_SIZE - 1] = paths[0]
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
    def test_missing_checkout_is_provisioned_from_local_repository_with_manifest_digest(self) -> None:
        manifest_path = Path(__file__).with_name("real-query-manifest.json")
        document = json.loads(manifest_path.read_text())
        expected = {
            row["evidence_tree_sha256"]
            for row in document["rows"]
            if "excluded_reason" not in row
        }
        self.assertEqual(len(expected), 1)
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "missing-evidence-tree"
            self.assertFalse(destination.exists())
            provision(manifest_path, ROOT, destination)
            self.assertTrue(destination.is_dir())
            self.assertEqual(evidence_tree_sha256(destination), next(iter(expected)))

    def test_mutated_evidence_tree_is_rejected_with_mismatch_fault(self) -> None:
        manifest_path = Path(__file__).with_name("real-query-manifest.json")
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "evidence-tree"
            provision(manifest_path, ROOT, destination)
            mutated = next(path for path in sorted(destination.rglob("*")) if path.is_file())
            original = mutated.read_bytes()
            mutated.write_bytes(original + b"\nmutated evidence\n")
            with self.assertRaisesRegex(InputFault, "corpus_vector_model_mismatch"):
                load_inputs(manifest_path, destination)

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
        self.assertEqual(
            client.calls,
            [
                {
                    "query": "recorded test visibility",
                    "topK": PAGE_SIZE,
                    "includeTests": True,
                }
            ],
        )
        score = {"profile": "single_page", "capability": capability, "rows": copy.deepcopy(rows)}
        score["rows"][0]["requests"][0]["offset"] = 0
        with self.assertRaisesRegex(InputFault, "request_bound_violation.*single_page"):
            validate_profile_score(score)

    def test_page_size_matches_product_search_schema_maximum(self) -> None:
        root = Path(__file__).resolve().parents[2]
        schema_path = root / "crates/aft/src/subc_tool_schemas.json"
        maximum = json.loads(schema_path.read_text())["search"]["properties"]["topK"]["maximum"]
        self.assertEqual(
            PAGE_SIZE,
            maximum,
            f"{schema_path.relative_to(root)} search.topK.maximum: {maximum}",
        )

    def test_invariance_plans_never_exceed_page_size(self) -> None:
        for plan_index, plan in enumerate(invariance_requests()):
            for request_index, request in enumerate(plan):
                with self.subTest(plan=plan_index, request=request_index):
                    self.assertLessEqual(request["topK"], PAGE_SIZE)

    def test_paged_profile_covers_frozen_depth_and_runs_invariance_requests(self) -> None:
        capability = {
            "schema_path": "fixture.json",
            "schema_sha256": "0" * 64,
            "offset_declared": True,
            "offset_bounds": {"minimum": 0, "maximum": 10000},
        }
        client = FakeClient(total=500)
        rows = score_manifest_rows(manifest(), "paged", capability, client, Path("."))
        expected_invariance_lengths = [10, 4, 2]
        self.assertEqual(rows[0]["pages_fetched"], D_0 // PAGE_SIZE)
        self.assertEqual(
            [len(plan) for plan in rows[0]["invariance_requests"]],
            expected_invariance_lengths,
        )
        self.assertEqual(
            rows[0]["request_count"],
            D_0 // PAGE_SIZE + sum(expected_invariance_lengths),
        )
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
        self.assertEqual(len(page_zero), PAGE_SIZE - 1)
        self.assertEqual(
            page_zero,
            [f"src/file{index:03}.py" for index in range(PAGE_SIZE - 1)],
        )
        self.assertNotIn(f"src/file{PAGE_SIZE - 1:03}.py", page_zero)

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


def _post_embeddings(port: int, texts: list[str]) -> tuple[int, dict[str, Any]]:
    body = json.dumps({"model": "aft-search-fixture-v1", "input": texts}).encode()
    request = urllib.request.Request(
        f"http://127.0.0.1:{port}/v1/embeddings", data=body, headers={"content-type": "application/json"}
    )
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        return error.code, json.load(error)


class VectorPackTests(unittest.TestCase):
    TEMPLATE = "aft-search-template-v1"

    def _pack(self, directory: Path) -> tuple[Path, dict[str, list[float]]]:
        vectors = {
            query_key("known query", self.TEMPLATE): [0.1, -0.2, 0.3],
            corpus_key("known chunk", self.TEMPLATE): [0.5, 0.25, -0.125],
        }
        path = directory / "pack.bin"
        write_pack(
            path,
            {"pinned_sha": EVIDENCE_SHA, "embed_template_version": self.TEMPLATE, "model_id": "test-model"},
            vectors,
        )
        return path, vectors

    def test_vector_pack_round_trips_float16_values_by_text_digest(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path, vectors = self._pack(Path(directory))
            pack = read_pack(path)
            self.assertEqual(pack["model_id"], "test-model")
            self.assertEqual(pack["dimension"], 3)
            self.assertEqual(set(pack["vectors"]), set(vectors))
            for key, vector in vectors.items():
                expected = list(struct.unpack("<3e", struct.pack("<3e", *vector)))
                self.assertEqual(pack["vectors"][key], expected)
            # 0.1 is not a float16 value, so the stored vector is the rounded one.
            self.assertNotEqual(pack["vectors"][query_key("known query", self.TEMPLATE)][0], 0.1)
            self.assertNotIn(query_key("known query", "another-template"), pack["vectors"])
            self.assertNotIn(query_key("unknown query", self.TEMPLATE), pack["vectors"])

    def test_vector_pack_rejects_a_file_that_is_not_a_pack(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path, _ = self._pack(Path(directory))
            truncated = Path(directory) / "truncated.bin"
            truncated.write_bytes(path.read_bytes()[:-1])
            with self.assertRaisesRegex(InputFault, "embedding_pack_length"):
                read_pack(truncated)
            legacy = Path(directory) / "legacy.json"
            legacy.write_bytes(canonical_json({"schema": "aft-search-vector-pack-v1", "vectors": {}}))
            with self.assertRaisesRegex(InputFault, "embedding_pack_format"):
                read_pack(legacy)

    def test_fixture_server_refuses_a_missing_vector_instead_of_inventing_one(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path, vectors = self._pack(Path(directory))
            pack = read_pack(path)
            server = Server(("127.0.0.1", 0), pack["vectors"], self.TEMPLATE, Path(directory) / "requests.log")
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                status, payload = _post_embeddings(server.server_port, ["known query", "known chunk"])
                self.assertEqual(status, 200)
                self.assertEqual(
                    [item["embedding"] for item in payload["data"]],
                    [pack["vectors"][key] for key in vectors],
                )
                status, payload = _post_embeddings(server.server_port, ["known query", "text with no vector"])
                self.assertEqual(status, 422)
                self.assertTrue(payload["error"].startswith("vector_missing:"))
                self.assertIn(query_key("text with no vector", self.TEMPLATE), payload["error"])
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=5)
            self.assertEqual(len(pack["vectors"]), 2)


if __name__ == "__main__":
    unittest.main()
