#!/usr/bin/env python3
from __future__ import annotations

import contextlib
import copy
import hashlib
import io
import json
import struct
import subprocess
import tempfile
import threading
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Mapping
from unittest import mock

from embedding_fixture_server import Server, corpus_key, query_key
from evidence_tree import evidence_tree_sha256
from provision_evidence import provision
from run_exact_recall import CorpusMissing, validate_corpus
import bench_rerank
from run_real_query import ROOT, NdjsonClient, assemble_score, attach_pinned_git_metadata, load_capability, load_inputs, parser as real_query_parser, score_manifest_rows, probe_pattern_capability
from search_quality_lib import (
    D_0,
    EVIDENCE_SHA,
    INVARIANCE_DEPTH,
    PAGE_INVARIANCE_FAILED_FIELD,
    PAGE_SIZE,
    REFERENCE_NOT_PAGE_INVARIANT_FIELD,
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


class PageSizeDependentClient(FakeClient):
    """An engine whose ranking changes with the page size: a 10-result page
    returns its files in reverse, like a result cap applied before sorting."""

    def __init__(self) -> None:
        super().__init__(total=500)

    def search(self, arguments: Mapping[str, Any]) -> dict[str, Any]:
        response = super().search(arguments)
        if int(arguments["topK"]) == 10:
            response["results"] = list(reversed(response["results"]))
        return response


def paged_capability() -> dict[str, Any]:
    return {
        "schema_path": "fixture.json",
        "schema_sha256": "0" * 64,
        "offset_declared": True,
        "offset_bounds": {"minimum": 0, "maximum": 10000},
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


class SplitQueryRunnerTests(unittest.TestCase):
    def split_manifest(self, pattern: str = "anchor") -> dict[str, Any]:
        document = manifest(False)
        document["rows"][0].update(pattern=pattern, answer_kind="concept", split_kind="R2")
        return document

    def test_partial_search_reply_is_allowed_only_for_building_fixture_client(self) -> None:
        from run_real_query import NdjsonClient, AftProtocolError
        client = object.__new__(NdjsonClient)
        client.call = lambda *args, **kwargs: {"success": True, "status": "building", "complete": False, "results": []}
        client.allow_building = False
        with self.assertRaises(AftProtocolError):
            client.search({"query": "question"})
        client.allow_building = True
        self.assertIs(client.search({"query": "question"})["complete"], False)
        client.call = lambda *args, **kwargs: {"success": True, "status": "failed", "results": []}
        with self.assertRaises(AftProtocolError):
            client.search({"query": "question"})

    def test_split_replay_refuses_a_vector_rejected_by_its_endpoint(self) -> None:
        from run_real_query import fixture_endpoint
        with tempfile.TemporaryDirectory() as directory:
            pack = {"embed_template_version": "test", "vectors": {query_key("known", "test"): [1.0]}}
            with self.assertRaisesRegex(InputFault, "vector_missing"):
                with fixture_endpoint(pack, Path(directory) / "log") as endpoint:
                    request = urllib.request.Request(endpoint + "/v1/embeddings", data=json.dumps({"input": "unknown"}).encode(), headers={"Content-Type": "application/json"})
                    with self.assertRaises(urllib.error.HTTPError):
                        urllib.request.urlopen(request, timeout=5)

    def test_building_fixture_holds_corpus_but_serves_query_embeddings(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            release = threading.Event()
            template = "test"
            vectors = {corpus_key("chunk", template): [1.0], query_key("question", template): [2.0], corpus_key("model probe", template): [3.0]}
            server = Server(("127.0.0.1", 0), vectors, template, Path(directory) / "log", corpus_release=release, unheld_corpus_texts={"model probe"})
            serving = threading.Thread(target=server.serve_forever, daemon=True)
            serving.start()
            replies = []
            def embed(text: str) -> dict:
                request = urllib.request.Request(f"http://127.0.0.1:{server.server_port}/v1/embeddings", data=json.dumps({"input": text}).encode(), headers={"Content-Type": "application/json"})
                with urllib.request.urlopen(request, timeout=5) as response:
                    return json.load(response)
            corpus = threading.Thread(target=lambda: replies.append(embed("chunk")), daemon=True)
            try:
                corpus.start()
                self.assertTrue(server.corpus_waiting.wait(3), "corpus embedding reached the held endpoint")
                self.assertEqual(embed("question")["data"][0]["embedding"], [2.0])
                self.assertEqual(embed("model probe")["data"][0]["embedding"], [3.0])
                self.assertEqual(replies, [], "semantic indexing must still be held")
            finally:
                release.set()
                corpus.join(5)
                server.shutdown()
                server.server_close()
                serving.join(5)
            self.assertEqual(replies[0]["data"][0]["embedding"], [1.0])

    def test_split_rows_cover_kinds_and_tuning_is_disjoint(self) -> None:
        gate = json.loads(Path(__file__).with_name("real-query-manifest.json").read_text())
        tuning = json.loads(Path(__file__).with_name("split-tuning-manifest.json").read_text())
        rows = [row for row in gate["rows"] if "pattern" in row]
        self.assertGreaterEqual(len(rows), 12)
        self.assertEqual({row["split_kind"] for row in rows}, {f"R{number}" for number in range(1, 8)})
        dual = [row for row in rows if row["split_kind"] == "R4"]
        self.assertEqual(len(dual), 2)
        self.assertEqual((dual[0]["query"], dual[0]["pattern"]), (dual[1]["query"], dual[1]["pattern"]))
        self.assertEqual({row["answer_kind"] for row in dual}, {"concept", "definition"})
        self.assertTrue(tuning["tuning_only"])
        self.assertGreaterEqual(len(tuning["rows"]), 8)
        self.assertTrue(all(row["tuning_only"] for row in tuning["rows"]))
        self.assertTrue({(row["query"], row["pattern"]) for row in rows}.isdisjoint({(row["query"], row["pattern"]) for row in tuning["rows"]}))
        self.assertTrue(all(row["row_source"] and row["answer_key_basis"] for row in rows + tuning["rows"]))
        from run_real_query import load_manifest_and_tree
        manifest_path = Path(__file__).with_name("real-query-manifest.json")
        # The self-test runs before the gate provisions the pinned evidence tree,
        # so materialize it here; provision() reuses an already-matching tree.
        with contextlib.redirect_stdout(io.StringIO()):
            provision(manifest_path)
        _, tree, _, _ = load_manifest_and_tree(manifest_path)
        import re
        broad = next(row for row in rows if row["split_kind"] == "R3")
        matched = [path for path in (tree / "crates/aft/src").rglob("*.rs") if re.search(broad["pattern"], path.read_text(), re.MULTILINE)]
        self.assertEqual(len(matched), broad["pattern_files"])
        self.assertIn(tree / broad["opened_file"], matched)
        self.assertTrue(all((tree / row["opened_file"]).is_file() for row in rows + tuning["rows"]))

    def test_candidate_chunk_texts_have_vectors_in_the_gate_supplement(self) -> None:
        # A pending engine change embeds these texts on the pinned tree; the
        # unchanged engine never does. Without their vectors the candidate's
        # replay faults with vector_missing instead of producing a score.
        from capture_split_query_vectors import CANDIDATE_CHUNK_TEXTS, candidate_chunk_texts
        texts = candidate_chunk_texts()
        self.assertTrue(texts)
        gate = json.loads(Path(__file__).with_name("real-query-manifest.json").read_text())
        main_pack = read_pack(ROOT / next(row for row in gate["rows"] if "excluded_reason" not in row)["embedding_pack"])
        supplement = read_pack(ROOT / gate["split_query_pack"]["path"])
        template = str(main_pack["embed_template_version"])
        for text in texts:
            key = corpus_key(text, template)
            self.assertIn(key, supplement["vectors"])
            self.assertNotIn(key, main_pack["vectors"])
        document = json.loads(CANDIDATE_CHUNK_TEXTS.read_text())
        document["texts"][0]["embed_text"] += " "
        with tempfile.TemporaryDirectory() as directory:
            tampered = Path(directory) / "texts.json"
            tampered.write_text(json.dumps(document))
            with self.assertRaisesRegex(InputFault, "candidate_chunk_text_digest_mismatch"):
                candidate_chunk_texts(tampered)

    def test_binary_capability_probe_classifies_invalid_pattern_and_ignored_input(self) -> None:
        class Probe:
            def __init__(self, response: dict[str, Any]):
                self.response = response
            def call(self, command: str, arguments: dict[str, Any]) -> dict[str, Any]:
                self.request = arguments
                return self.response
        for response, expected in (({"success": False, "code": "invalid_pattern"}, True), ({"success": True}, False)):
            client = Probe(response)
            self.assertEqual(probe_pattern_capability(client)["pattern_declared"], expected)
            self.assertEqual(client.request["arguments"]["pattern"], "[")
        with self.assertRaisesRegex(InputFault, "pattern_capability_probe_failed"):
            probe_pattern_capability(Probe({"success": False, "code": "unrelated"}))

    def test_split_parameters_and_prose_pair_are_separate(self) -> None:
        client = FakeClient()
        capability = {"offset_declared": False, "pattern_declared": True}
        rows = score_manifest_rows(self.split_manifest(), "single_page", capability, client, Path("/fixture"))
        self.assertEqual(client.calls[0]["query"], "recorded test visibility")
        self.assertEqual(client.calls[0]["pattern"], "anchor")
        self.assertNotIn("pattern", client.calls[1])
        self.assertEqual(rows[0]["input_form"], "split")
        self.assertEqual(rows[0]["prose_only"]["requests"], [client.calls[1]])

    def test_legacy_join_is_recorded_and_empty_pattern_does_not_add_space(self) -> None:
        capability = {"offset_declared": False, "pattern_declared": False}
        for pattern, expected in (("anchor", "recorded test visibility anchor"), ("   ", "recorded test visibility")):
            client = FakeClient()
            rows = score_manifest_rows(self.split_manifest(pattern), "single_page", capability, client, Path("/fixture"))
            self.assertEqual(client.calls[0]["query"], expected)
            self.assertNotIn("pattern", client.calls[0])
            self.assertEqual(rows[0]["input_form"], "joined")

    def test_empty_pattern_result_drift_is_refused(self) -> None:
        class Drifting(FakeClient):
            def search(self, arguments: Mapping[str, Any]) -> dict[str, Any]:
                response = super().search(arguments)
                if "pattern" in arguments:
                    response["results"].reverse()
                return response
        document = self.split_manifest("")
        document["rows"][0]["split_kind"] = "R5"
        with self.assertRaisesRegex(InputFault, "empty_pattern_not_identical"):
            score_manifest_rows(document, "single_page", {"pattern_declared": True}, Drifting(), Path("/fixture"))

    def assembled(self, capability: dict[str, Any]) -> dict[str, Any]:
        document = self.split_manifest()
        rows = score_manifest_rows(document, "single_page", capability, FakeClient(), Path("/fixture"))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest_path = root / "manifest.json"
            manifest_path.write_bytes(canonical_json(document))
            binary = root / "aft"
            binary.write_bytes(b"fake-aft-binary")
            return assemble_score(document, rows, "single_page", capability, "fixture-model", exact_report(), concept_report(), manifest_path, binary, root / "missing-reference.json")

    def test_score_names_split_rows_an_engine_ignoring_pattern_cannot_be_judged_on(self) -> None:
        base = {"schema_path": "fixture.json", "schema_sha256": "0" * 64, "offset_declared": False}
        ignored = self.assembled(base | {"pattern_declared": False, "pattern_probe": "ignored_pattern"})
        self.assertEqual(ignored["split_rows_not_applicable"]["rows"], [ignored["rows"][0]["episode_id"]])
        self.assertIn("ignored_pattern", ignored["split_rows_not_applicable"]["reason"])
        honoured = self.assembled(base | {"pattern_declared": True, "pattern_probe": "invalid_pattern"})
        self.assertNotIn("split_rows_not_applicable", honoured)


class CapabilitySchemaTests(unittest.TestCase):
    SCHEMA_BLOCK = '''const SearchParams = Type.Object(
  {
    query: Type.Optional(Type.String({ description: "Search text" })),
    offset: Type.Optional(Type.Integer({ minimum: 0, maximum: 100000 })),
  },
  { additionalProperties: false },'''
    SOURCE = "// Plugin imports\n" + SCHEMA_BLOCK + "\n);\nfunction renderCall(args) { return args.query; }\n"
    JSON_SCHEMA = {
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "Search text"},
            "offset": {"type": "integer", "minimum": 0, "maximum": 100000},
        },
    }

    def capability(self, source: str) -> dict[str, Any]:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "schema"
            path.write_bytes(source.encode("utf-8"))
            return load_capability(path)

    def digest(self, source: str) -> str:
        return str(self.capability(source)["schema_sha256"])

    def test_typescript_digest_hashes_only_the_extracted_schema_bytes(self) -> None:
        self.assertEqual(self.digest(self.SOURCE), hashlib.sha256(self.SCHEMA_BLOCK.encode("utf-8")).hexdigest())
        capability = self.capability(self.SOURCE)
        self.assertTrue(capability["offset_declared"])
        self.assertEqual(capability["offset_bounds"], {"minimum": 0, "maximum": 100000})

    def test_typescript_edit_outside_schema_leaves_digest_unchanged(self) -> None:
        changed = self.SOURCE.replace("return args.query;", "return typeof args.query === 'string' ? args.query : '';")
        self.assertNotEqual(self.SOURCE, changed)
        self.assertEqual(self.digest(self.SOURCE), self.digest(changed))

    def test_typescript_edits_inside_schema_change_digest(self) -> None:
        for label, old, new in (
            ("new parameter", "    query:", "    pattern: Type.Optional(Type.String()),\n    query:"),
            ("changed bound", "maximum: 100000", "maximum: 200000"),
            ("changed description", "Search text", "Search text or regex"),
        ):
            with self.subTest(edit=label):
                changed = self.SOURCE.replace(old, new)
                self.assertNotEqual(self.SOURCE, changed)
                self.assertNotEqual(self.digest(self.SOURCE), self.digest(changed))

    def test_typescript_crlf_and_lf_have_the_same_digest(self) -> None:
        self.assertEqual(self.digest(self.SOURCE), self.digest(self.SOURCE.replace("\n", "\r\n")))

    def test_typescript_schema_whitespace_is_not_normalized(self) -> None:
        changed = self.SOURCE.replace("    query:", "      query:")
        self.assertNotEqual(self.digest(self.SOURCE), self.digest(changed))

    def test_json_digest_hashes_canonical_schema_not_file_formatting(self) -> None:
        compact = json.dumps(self.JSON_SCHEMA, separators=(",", ":"))
        reordered = {"properties": dict(reversed(list(self.JSON_SCHEMA["properties"].items()))), "type": "object"}
        formatted = json.dumps(reordered, indent=2)
        expected = hashlib.sha256(canonical_json(self.JSON_SCHEMA)).hexdigest()
        self.assertEqual(self.digest(compact), expected)
        self.assertEqual(self.digest(formatted), expected)
        self.assertEqual(self.digest(formatted.replace("\n", "\r\n")), expected)

    def test_json_schema_edits_change_digest(self) -> None:
        original = self.digest(json.dumps(self.JSON_SCHEMA))
        for label, name, value in (
            ("new parameter", "pattern", {"type": "string"}),
            ("changed bound", "offset", {"type": "integer", "minimum": 0, "maximum": 200000}),
            ("changed description", "query", {"type": "string", "description": "Search text or regex"}),
        ):
            with self.subTest(edit=label):
                changed = copy.deepcopy(self.JSON_SCHEMA)
                changed["properties"][name] = value
                self.assertNotEqual(original, self.digest(json.dumps(changed)))


class ViewsOptionTests(unittest.TestCase):
    def test_real_query_views_flag_is_opt_in(self) -> None:
        self.assertFalse(real_query_parser().parse_args([]).views)
        self.assertTrue(real_query_parser().parse_args(["--views"]).views)

    def test_views_runtime_metadata_points_to_the_pinned_evidence_head(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "tree"
            root.mkdir()
            (root / "fixture.py").write_text("def fixture(): pass\n")
            attach_pinned_git_metadata(root)
            head = subprocess.run(
                ["git", "rev-parse", "HEAD"], cwd=root, text=True, capture_output=True, check=True
            )
            entries = subprocess.run(
                ["git", "ls-tree", "-r", "-z", "HEAD"], cwd=root, capture_output=True, check=True
            )
        self.assertEqual(head.stdout.strip(), EVIDENCE_SHA)
        self.assertIn(b"crates/aft/src/config.rs", entries.stdout)

    def configure_document(self, views_enabled: bool) -> str:
        captured: dict[str, Any] = {}
        client = object.__new__(NdjsonClient)
        client.project_root = Path("/fixture/project")
        client.storage_dir = Path("/fixture/storage")

        def call(
            command: str,
            params: Mapping[str, Any] | None = None,
            timeout: float = 120.0,
        ) -> dict[str, Any]:
            self.assertEqual(command, "configure")
            captured.update(params or {})
            return {"success": True}

        with (
            mock.patch.dict("os.environ", {bench_rerank.CONFIG_ENV: ""}),
            mock.patch.object(client, "call", side_effect=call),
        ):
            client.configure(None, "fixture-model", 4.5, views_enabled=views_enabled)
        return str(captured["config"][0]["doc"])

    def test_default_configure_document_is_byte_identical_and_views_adds_only_its_key(self) -> None:
        default = self.configure_document(False)
        self.assertEqual(
            default,
            '{"indexes": {"trigram": true, "semantic": true, "callgraph": false}, '
            '"semantic": {"backend": "fastembed", "model": "fixture-model", '
            '"timeout_ms": 4500, "query_timeout_ms": 4500, "max_batch_size": 64, "max_files": 20000}}',
        )

        enabled = self.configure_document(True)
        default_value = json.loads(default)
        enabled_value = json.loads(enabled)
        self.assertEqual(set(enabled_value) - set(default_value), {"views"})
        self.assertEqual(enabled_value["views"], {"enabled": True})

    def test_missing_views_work_counters_are_unavailable_not_zero(self) -> None:
        client = object.__new__(NdjsonClient)
        self.assertEqual(
            client.views_work(),
            {
                "status": "unavailable",
                "reason": "standalone_ndjson_status_and_health_digest_do_not_expose_metrics.views_work",
            },
        )

    def test_wait_ready_waits_for_pinned_quiescent_views_without_sleeping(self) -> None:
        client = object.__new__(NdjsonClient)
        client.views_enabled = True
        statuses = [
            {
                "search_index": {"status": "ready"},
                "semantic_index": {"status": "ready"},
                "views": {"pinned": True, "pending_paths": 4, "failed_paths": 0},
            },
            {
                "search_index": {"status": "ready"},
                "semantic_index": {"status": "ready"},
                "views": {"pinned": True, "pending_paths": 0, "failed_paths": 0},
            },
        ]
        with (
            mock.patch.object(client, "call", side_effect=statuses) as call,
            mock.patch("run_real_query.time.sleep", side_effect=AssertionError("views readiness must not sleep")),
            mock.patch("run_real_query.bench_rerank.settle", side_effect=AssertionError("views readiness uses status")),
        ):
            client.wait_ready(5.0)
        self.assertEqual(call.call_count, 2)


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

    def test_unflagged_row_breaking_page_invariance_faults(self) -> None:
        with self.assertRaisesRegex(InputFault, r"^page_invariance_failed:followup-census:1$"):
            score_manifest_rows(manifest(), "paged", paged_capability(), PageSizeDependentClient(), Path("."))

    def test_flagged_row_breaking_page_invariance_records_a_miss(self) -> None:
        document = manifest()
        document["rows"][0]["opened_file"] = "src/file000.py"
        document["rows"][0][REFERENCE_NOT_PAGE_INVARIANT_FIELD] = "defect under measurement"
        invariant = score_manifest_rows(document, "paged", paged_capability(), FakeClient(total=500), Path("."))
        self.assertEqual(invariant[0]["metrics"]["mrr_at_10"], 1.0)
        self.assertNotIn(PAGE_INVARIANCE_FAILED_FIELD, invariant[0])
        rows = score_manifest_rows(document, "paged", paged_capability(), PageSizeDependentClient(), Path("."))
        self.assertEqual(rows[0][PAGE_INVARIANCE_FAILED_FIELD], "page_invariance_failed:followup-census:1")
        self.assertEqual(rows[0]["metrics"], {"mrr_at_10": 0.0, "hit_at_1": 0.0, "hit_at_5": 0.0})
        self.assertEqual((rows[0]["ranked_paths"], rows[0]["page_zero_ranked_paths"], rows[0]["retrieval_depth"]), ([], [], 0))
        validate_profile_score({"profile": "paged", "capability": paged_capability() | {"probe_pages_differ": True}, "rows": rows})

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
