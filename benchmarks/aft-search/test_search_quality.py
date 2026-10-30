#!/usr/bin/env python3
from __future__ import annotations

import argparse
import copy
import json
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

from run_real_query import load_capability
from run_search_quality import parser as quality_runner_parser, run as quality_runner_run, selected_profile
from search_quality import (
    diff_paths,
    run as quality_run,
    parser as quality_parser,
    descriptor_labels,
    page_zero_evaluation_projection,
    synthetic_documents,
)
from search_quality_lib import (
    PAGE_INVARIANCE_FAILED_FIELD,
    PAGE_SIZE,
    REFERENCE_NOT_PAGE_INVARIANT_FIELD,
    SPLIT_ROWS_NOT_APPLICABLE_REASON,
    SEMANTIC_BUILDING_NOT_APPLICABLE_FIELD,
    InputFault,
    TOOL_CALL_PARITY_FIXTURE_SOURCE,
    UNREACHABLE_SPLIT_ROWS_FIELD,
    apply_unreachable_split_rows,
    aggregate_real_query,
    derive_slice_class,
    included_manifest_ids,
    real_query_behavior_diff,
    read_tool_schema_blobs,
    resolve_descriptor,
    split_paired_failures,
    split_rows_not_applicable,
    evaluate_predicate,
    mean_metrics,
    row_metrics,
    total_gate,
    validate_included_row_mechanisms,
    validate_manifest_maintenance_scores,
    validate_manifest_relabels,
    validate_profile_score,
    validate_scored_population,
    validate_unreachable_split_rows,
)


class SplitPairedTests(unittest.TestCase):
    def candidate(self, combined: float, prose: float, form: str = "split", kind: str = "concept") -> dict:
        return {"rows": [{"episode_id": "followup-census:910003", "input_form": form, "answer_kind": kind,
            "metrics": {"mrr_at_10": combined, "hit_at_1": 0.0, "hit_at_5": 1.0},
            "prose_only": {"metrics": {"mrr_at_10": prose, "hit_at_1": 0.0, "hit_at_5": 1.0}}}]}

    def test_paired_check_fails_harmed_concept_row(self) -> None:
        failures = split_paired_failures(self.candidate(0.25, 0.5))
        self.assertEqual(failures, ["split_paired_harm:followup-census:910003:mrr_delta=-0.250000"])

    def test_paired_check_passes_improved_concept_row(self) -> None:
        self.assertEqual(split_paired_failures(self.candidate(0.5, 0.25)), [])

    def test_paired_check_does_not_protect_definition_answer(self) -> None:
        self.assertEqual(split_paired_failures(self.candidate(0.25, 0.5, kind="definition")), [])

    def test_joined_form_cannot_be_evaluated_as_split(self) -> None:
        with self.assertRaisesRegex(InputFault, "split_candidate_required.*joined"):
            split_paired_failures(self.candidate(0.5, 0.25, form="joined"))

    def ranking_score_with_probe(self, pattern_declared: bool, pattern_probe: str) -> tuple[dict, dict]:
        """A ranking evaluation whose only split row is joined and harmed."""
        _, reference, score = synthetic_documents()
        score["rows"] = self.candidate(0.25, 0.5, form="joined")["rows"]
        score["rows"][0]["split_kind"] = "R1"
        score["mechanisms"]["topk_cut"]["mrr_at_10"] = 0.6
        score["capability"].update(pattern_declared=pattern_declared, pattern_probe=pattern_probe)
        return reference, score

    def test_engine_ignoring_pattern_skips_joined_split_rows_under_ranking(self) -> None:
        reference, score = self.ranking_score_with_probe(False, "ignored_pattern")
        descriptor = {"slice_class": "ranking", "targeted_mechanism": "topk_cut"}
        self.assertEqual(evaluate_predicate(reference, score, descriptor), [])
        self.assertEqual(
            split_rows_not_applicable(score),
            {"reason": SPLIT_ROWS_NOT_APPLICABLE_REASON, "rows": ["followup-census:910003"]},
        )

    def test_engine_honouring_pattern_still_fails_a_missing_split_candidate(self) -> None:
        reference, score = self.ranking_score_with_probe(True, "invalid_pattern")
        descriptor = {"slice_class": "ranking", "targeted_mechanism": "topk_cut"}
        self.assertIsNone(split_rows_not_applicable(score))
        with self.assertRaisesRegex(InputFault, "split_candidate_required:followup-census:910003:joined"):
            evaluate_predicate(reference, score, descriptor)

    def test_skip_needs_an_explicit_ignored_pattern_probe(self) -> None:
        # A score with no probe result, or a probe that contradicts the
        # declared capability, is judged in full.
        for capability in ({}, {"pattern_declared": False}, {"pattern_declared": True, "pattern_probe": "ignored_pattern"}):
            score = self.candidate(0.5, 0.25, form="joined")
            score["capability"] = capability
            self.assertIsNone(split_rows_not_applicable(score), capability)
            with self.assertRaisesRegex(InputFault, "split_candidate_required"):
                split_paired_failures(score)

    def test_skip_annotation_is_ignored_by_behaviour_comparison(self) -> None:
        _, reference, score = synthetic_documents()
        named = copy.deepcopy(score)
        named["split_rows_not_applicable"] = {"reason": SPLIT_ROWS_NOT_APPLICABLE_REASON, "rows": ["followup-census:910003"]}
        self.assertEqual(real_query_behavior_diff(reference, named), real_query_behavior_diff(reference, score))

    def test_ranking_predicate_invokes_paired_check(self) -> None:
        _, reference, score = synthetic_documents()
        score["rows"] = self.candidate(0.25, 0.5)["rows"]
        score["mechanisms"]["topk_cut"]["mrr_at_10"] = 0.6
        descriptor = {"slice_class": "ranking", "targeted_mechanism": "topk_cut"}
        self.assertIn("split_paired_harm:followup-census:910003:mrr_delta=-0.250000", evaluate_predicate(reference, score, descriptor))

    def test_population_check_refuses_joined_row_relabelled_as_split(self) -> None:
        from test_run_real_query import FakeClient, manifest
        from run_real_query import score_manifest_rows
        document = manifest(False)
        document["rows"][0].update(pattern="anchor", answer_kind="concept", split_kind="R2")
        capability = {"pattern_declared": False}
        rows = score_manifest_rows(document, "single_page", capability, FakeClient(), Path("/fixture"))
        score = {"rows": rows, "capability": capability}
        validate_scored_population(document, score)
        rows[0]["input_form"] = "split"
        with self.assertRaisesRegex(InputFault, "split_input_form_mismatch"):
            validate_scored_population(document, score)

    def test_population_check_refuses_forged_prose_pair(self) -> None:
        from test_run_real_query import FakeClient, manifest
        from run_real_query import score_manifest_rows
        document = manifest(False)
        document["rows"][0].update(pattern="anchor", answer_kind="concept", split_kind="R2")
        capability = {"pattern_declared": True}
        rows = score_manifest_rows(document, "single_page", capability, FakeClient(), Path("/fixture"))
        rows[0]["prose_only"]["requests"][0]["query"] += " anchor"
        with self.assertRaisesRegex(InputFault, "split_prose_pair_mismatch"):
            validate_scored_population(document, {"rows": rows, "capability": capability})

    def test_split_kind_answer_constraints_are_enforced(self) -> None:
        for kind, expected in (("R1", "split_rank1_required"), ("R3", "split_hit5_required"), ("R4", "split_hit3_required"), ("R6", "split_partial_hit_required")):
            score = self.candidate(0.0, 0.0)
            score["rows"][0]["split_kind"] = kind
            score["rows"][0]["metrics"]["hit_at_5"] = 0.0
            self.assertTrue(any(item.startswith(expected) for item in split_paired_failures(score)), kind)
        partial = self.candidate(0.5, 0.25, kind="definition")
        partial["rows"][0].update(split_kind="R6", envelope_complete=False)
        self.assertEqual(split_paired_failures(partial), [])

    def test_tuning_score_is_not_gate_eligible(self) -> None:
        document, _, score = synthetic_documents()
        score["tuning_only"] = True
        with self.assertRaisesRegex(InputFault, "tuning_only_score_not_gate_eligible"):
            validate_scored_population(document, score)

    def test_tuning_manifest_is_not_gate_eligible(self) -> None:
        with self.assertRaisesRegex(InputFault, "tuning_only_manifest_not_gate_eligible"):
            included_manifest_ids(json.loads(Path(__file__).with_name("split-tuning-manifest.json").read_text()))


class MeanMetricsTests(unittest.TestCase):
    """Aggregates must not depend on row order or on how a Python build sums floats.

    The gate compares aggregates exactly against the reference. A reference
    recorded by one Python differed from a CI score in the last bit
    (0.22023809523809526 against 0.22023809523809523) over identical rows, and
    the non-ranking gate refused it as a regression.
    """

    def test_mean_is_independent_of_row_order(self) -> None:
        values = [1e16, 1.0, -1e16, 0.1, 0.2, 0.3]
        rows = [{"mrr_at_10": value, "hit_at_1": 0.0, "hit_at_5": 0.0} for value in values]
        forward = mean_metrics(rows)["mrr_at_10"]
        backward = mean_metrics(list(reversed(rows)))["mrr_at_10"]
        self.assertEqual(forward, backward)
        self.assertEqual(forward, 1.6 / 6)


class ManifestMaintenanceScoreTests(unittest.TestCase):
    """The old and new scores of a manifest re-record must share binary and profile."""

    def manifest(self, packs: list[str]) -> dict:
        rows = [
            {"episode_id": f"followup-census:{index}", "embedding_pack_sha256": pack}
            for index, pack in enumerate(packs, 1)
        ]
        rows.append({"episode_id": "followup-census:99", "excluded_reason": "repo_unowned_or_unavailable"})
        return {"rows": rows}

    def score(self, model: str, binary: str = "b" * 64, profile: str = "paged") -> dict:
        return {"model_id": model, "binary_sha256": binary, "profile": profile}

    def test_same_pack_with_a_different_model_is_refused(self) -> None:
        with self.assertRaisesRegex(InputFault, "model_changed_without_pack_change"):
            validate_manifest_maintenance_scores(
                self.manifest(["p", "p"]), self.manifest(["p", "p"]), self.score("old"), self.score("new")
            )

    def test_a_different_binary_is_refused_even_with_the_same_model(self) -> None:
        with self.assertRaisesRegex(InputFault, "manifest_maintenance_binary_profile_mismatch"):
            validate_manifest_maintenance_scores(
                self.manifest(["p"]), self.manifest(["p"]), self.score("m"), self.score("m", binary="c" * 64)
            )
        with self.assertRaisesRegex(InputFault, "manifest_maintenance_binary_profile_mismatch"):
            validate_manifest_maintenance_scores(
                self.manifest(["p"]), self.manifest(["p"]), self.score("m"), self.score("m", profile="single_page")
            )

    def test_a_pack_change_on_every_row_allows_a_different_model(self) -> None:
        validate_manifest_maintenance_scores(
            self.manifest(["old", "old"]), self.manifest(["new", "new"]), self.score("fixture"), self.score("minilm")
        )

    def test_a_pack_change_on_some_rows_only_is_refused(self) -> None:
        with self.assertRaisesRegex(InputFault, "model_changed_without_pack_change"):
            validate_manifest_maintenance_scores(
                self.manifest(["old", "old"]), self.manifest(["new", "old"]), self.score("fixture"), self.score("minilm")
            )


class ManifestMechanismTests(unittest.TestCase):
    def manifest(self) -> dict:
        return {
            "rows": [
                {
                    "episode_id": "followup-census:1",
                    "mechanism": "phrase_present_not_surfaced",
                },
                {
                    "episode_id": "followup-census:2",
                    "mechanism": "not_a_search_failure",
                },
            ]
        }

    def test_included_rows_have_one_mechanism_each(self) -> None:
        manifest = self.manifest()
        validate_included_row_mechanisms(manifest)
        for invalid in (None, ["phrase_present_not_surfaced", "not_a_search_failure"]):
            changed = copy.deepcopy(manifest)
            changed["rows"][0]["mechanism"] = invalid
            with self.subTest(invalid=invalid):
                with self.assertRaisesRegex(InputFault, "malformed_schema:mechanism"):
                    validate_included_row_mechanisms(changed)

    def test_relabelled_row_carries_reason_without_changing_population(self) -> None:
        old_manifest = self.manifest()
        new_manifest = copy.deepcopy(old_manifest)
        new_manifest["rows"][0]["mechanism"] = "not_a_search_failure"
        new_manifest["rows"][0]["relabel_reason"] = (
            "pinned_sha=30d4a64f99b3b15fd88be6cf962fff4b3fe5ea17: wiring=0"
        )
        validate_manifest_relabels(old_manifest, new_manifest)
        self.assertEqual(included_manifest_ids(old_manifest), included_manifest_ids(new_manifest))

        del new_manifest["rows"][0]["relabel_reason"]
        with self.assertRaisesRegex(InputFault, "manifest_relabel_reason_missing"):
            validate_manifest_relabels(old_manifest, new_manifest)

    def test_corrected_phrase_rows_are_relabelled_with_reasons(self) -> None:
        path = Path(__file__).with_name("real-query-manifest.json")
        manifest = json.loads(path.read_text())
        rows = {row["episode_id"]: row for row in manifest["rows"]}
        expected = {
            "followup-census:832": ("NDJSON=0", "dispatch=0"),
            "followup-census:19696": ("cortexkit-store=0",),
            "followup-census:7695": ("wiring=0",),
        }
        for episode_id, missing_tokens in expected.items():
            with self.subTest(episode_id=episode_id):
                row = rows[episode_id]
                self.assertEqual(row["mechanism"], "not_a_search_failure")
                reason = row["relabel_reason"]
                self.assertIn("30d4a64f99b3b15fd88be6cf962fff4b3fe5ea17", reason)
                for missing_token in missing_tokens:
                    self.assertIn(missing_token, reason)


class ProfileSelectionTests(unittest.TestCase):
    def profile_for_schema(self, schema: dict) -> tuple[str, dict]:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            schema_path = root / "semantic.json"
            reference_path = root / "reference.json"
            schema_path.write_text(json.dumps(schema))
            reference_path.write_text(json.dumps({"profile": "single_page"}))
            args = argparse.Namespace(
                profile=None,
                mode="evaluate",
                rebaseline=False,
                to_profile=None,
                schema=str(schema_path),
                reference=str(reference_path),
            )
            profile = selected_profile(args)
            return profile, load_capability(schema_path)

    def test_offset_declaring_head_selects_paged_and_runs_probe(self) -> None:
        profile, capability = self.profile_for_schema(
            {
                "properties": {
                    "offset": {"type": "integer", "minimum": 0, "maximum": 100_000}
                }
            }
        )
        if profile == "paged":
            capability["probe_pages_differ"] = True
        validate_profile_score({"profile": profile, "capability": capability, "rows": []})
        self.assertEqual(profile, "paged")

    def test_no_offset_head_keeps_single_page_reference_profile(self) -> None:
        profile, capability = self.profile_for_schema({"properties": {}})
        self.assertEqual(profile, "single_page")
        validate_profile_score({"profile": profile, "capability": capability, "rows": []})


class ViewsOptionPropagationTests(unittest.TestCase):
    def test_views_flag_is_forwarded_to_real_query_only_when_enabled(self) -> None:
        for enabled in (False, True):
            with self.subTest(enabled=enabled), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                argv = [
                    "--corpus", str(root / "corpus.toml"),
                    "--binary", str(root / "aft"),
                    "--profile", "single_page",
                    "--score-output", str(root / "score.json"),
                ]
                if enabled:
                    argv.append("--views")
                args = quality_runner_parser().parse_args(argv)
                commands: list[list[str]] = []

                with (
                    mock.patch("run_search_quality.parse_corpus_toml", return_value=({}, [])),
                    mock.patch("run_search_quality.clone_root_for", return_value=root / "clones"),
                    mock.patch("run_search_quality.ensure_binary"),
                    mock.patch("run_search_quality.command", side_effect=lambda arguments, **kwargs: commands.append(list(arguments))),
                ):
                    quality_runner_run(args)

                real_query = next(command for command in commands if command[1].endswith("run_real_query.py"))
                self.assertEqual("--views" in real_query, enabled)


class SemanticBuildingNotApplicableTests(unittest.TestCase):
    def documents(self) -> tuple[dict, dict, dict, dict]:
        metrics = {"mrr_at_10": 1.0, "hit_at_1": 1.0, "hit_at_5": 1.0}
        scored_row = {
            "episode_id": "followup-census:1",
            "request": {"query": "q", "topK": PAGE_SIZE, "includeTests": False},
            "requests": [{"query": "q", "topK": PAGE_SIZE, "includeTests": False}],
            "request_count": 1,
            "include_tests": False,
            "include_tests_source": "default",
            "pages_fetched": 1,
            "collapse_stop_reason": "exhausted",
            "retrieval_depth": 1,
            "ranked_paths": ["src/opened.py"],
            "metrics": dict(metrics),
            "pinned_shape": "identifier",
            "mechanism": "topk_cut",
            "census_stratum": "identifier",
        }
        building_manifest_row = {
            "episode_id": "followup-census:2",
            "query": "q",
            "opened_file": "src/building.py",
            "include_tests": False,
            "include_tests_source": "default",
            "mechanism": "topk_cut",
            "semantic_state": "building",
        }
        manifest = {
            "rows": [
                {
                    "episode_id": "followup-census:1",
                    "query": "q",
                    "opened_file": "src/opened.py",
                    "include_tests": False,
                    "include_tests_source": "default",
                    "mechanism": "topk_cut",
                },
                building_manifest_row,
            ]
        }

        def document(rows: list[dict]) -> dict:
            aggregate = aggregate_real_query(rows)
            return {
                "schema": "aft-search-score-v1",
                "model_id": "fixture",
                "profile": "single_page",
                "views_enabled": True,
                "capability": {
                    "schema_path": "fixture.json",
                    "schema_sha256": "0" * 64,
                    "offset_declared": False,
                },
                "families": {
                    "exact_recall": dict(metrics),
                    "concept_recall": dict(metrics),
                    "real_query": aggregate["family"],
                },
                "fixture_groups": {"exact_recall": {}, "concept_recall": {}},
                "paired_deltas": {},
                "shapes": aggregate["shapes"],
                "mechanisms": aggregate["mechanisms"],
                "census_weighted_mrr_report_only": aggregate[
                    "census_weighted_mrr_report_only"
                ],
                "fixture_results": {"harness-goldens": True},
                "rows": copy.deepcopy(rows),
            }

        reference_building_row = copy.deepcopy(scored_row)
        reference_building_row.update(
            episode_id=building_manifest_row["episode_id"],
            request={"query": "q", "topK": PAGE_SIZE, "includeTests": False},
            requests=[{"query": "q", "topK": PAGE_SIZE, "includeTests": False}],
            ranked_paths=["src/wrong.py"],
            metrics={"mrr_at_10": 0.0, "hit_at_1": 0.0, "hit_at_5": 0.0},
        )
        reference = document([scored_row, reference_building_row])
        candidate = document([scored_row])
        placeholder = {
            "episode_id": building_manifest_row["episode_id"],
            "semantic_state": "building",
            "not_applicable_reason": "views-enabled semantic index was ready before the held-embedding fixture reached the building state",
        }
        candidate["rows"].append(placeholder)
        candidate[SEMANTIC_BUILDING_NOT_APPLICABLE_FIELD] = {
            "reason": placeholder["not_applicable_reason"],
            "rows": [placeholder["episode_id"]],
        }
        descriptor = {
            "kind": "harness",
            "slice_class": "non_ranking",
            "targeted_mechanism": "none",
            "fixtures": ["harness-goldens"],
        }
        return manifest, reference, candidate, descriptor

    def test_views_score_with_building_not_applicable_rows_is_accepted(self) -> None:
        manifest, reference, candidate, descriptor = self.documents()
        self.assertIs(candidate["views_enabled"], True)
        result = total_gate(reference, candidate, manifest, descriptor, [])
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(
            result.notes,
            (
                "semantic_building_rows_skipped:followup-census:2:"
                "views-enabled semantic index was ready before the held-embedding fixture reached the building state",
            ),
        )

    def test_non_views_score_with_building_not_applicable_rows_is_refused(self) -> None:
        manifest, reference, candidate, descriptor = self.documents()
        candidate.pop("views_enabled")
        result = total_gate(reference, candidate, manifest, descriptor, [])
        self.assertEqual(result.exit_code, 2)
        self.assertEqual(
            result.reasons,
            ("semantic_building_rows_not_applicable_requires_views_enabled:true",),
        )

    def test_non_building_rows_cannot_be_excluded_as_not_applicable(self) -> None:
        manifest, _reference, candidate, _descriptor = self.documents()
        manifest["rows"][1].pop("semantic_state")
        with self.assertRaisesRegex(InputFault, "semantic_building_not_applicable_mismatch"):
            validate_scored_population(manifest, candidate)


class PageZeroProjectionTests(unittest.TestCase):
    def documents(self) -> tuple[dict, dict, dict, dict]:
        manifest = {"rows": [{"episode_id": "episode:1", "opened_file": "src/opened.py"}]}
        metrics = {"mrr_at_10": 1.0, "hit_at_1": 1.0, "hit_at_5": 1.0}
        reference = {
            "profile": "single_page",
            "capability": {"offset_declared": False},
            "families": {"real_query": metrics},
            "shapes": {"mixed": metrics},
            "mechanisms": {"other": metrics},
            "rows": [
                {
                    "episode_id": "episode:1",
                    "pinned_shape": "mixed",
                    "mechanism": "other",
                    "census_stratum": "short",
                    "ranked_paths": ["src/opened.py"],
                    "metrics": metrics,
                }
            ],
        }
        score = {
            "profile": "paged",
            "capability": {
                "offset_declared": True,
                "probe_pages_differ": True,
            },
            "families": {"real_query": metrics},
            "shapes": {"mixed": metrics},
            "mechanisms": {"other": metrics},
            "rows": [
                {
                    "episode_id": "episode:1",
                    "pinned_shape": "mixed",
                    "mechanism": "other",
                    "census_stratum": "short",
                    "request": {"topK": PAGE_SIZE, "offset": 0},
                    "requests": [
                        {"topK": PAGE_SIZE, "offset": 0},
                        {"topK": PAGE_SIZE, "offset": PAGE_SIZE},
                    ],
                    "ranked_paths": ["src/opened.py", "src/later.py"],
                    "page_zero_ranked_paths": ["src/opened.py"],
                    "metrics": metrics,
                }
            ],
        }
        descriptor = {"slice_class": "ranking"}
        return manifest, reference, score, descriptor

    def test_projection_uses_only_page_zero_and_ignores_later_page_changes(self) -> None:
        manifest, reference, score, descriptor = self.documents()
        first = page_zero_evaluation_projection(reference, score, manifest, descriptor)
        score["rows"][0]["ranked_paths"][-1] = "src/different-later.py"
        second = page_zero_evaluation_projection(reference, score, manifest, descriptor)
        self.assertEqual(first["rows"][0]["ranked_paths"], reference["rows"][0]["ranked_paths"])
        self.assertEqual(first["rows"][0]["metrics"], reference["rows"][0]["metrics"])
        self.assertEqual(first["rows"][0]["metrics"], second["rows"][0]["metrics"])

    def test_projection_requires_a_successful_declared_offset_probe(self) -> None:
        manifest, reference, score, descriptor = self.documents()
        score["capability"]["probe_pages_differ"] = False
        with self.assertRaisesRegex(InputFault, "reference_profile_mismatch"):
            page_zero_evaluation_projection(reference, score, manifest, descriptor)


class ToolSchemaFenceTests(unittest.TestCase):
    path = "crates/aft/src/subc_tool_schemas.json"

    def setUp(self) -> None:
        self.base: dict[str, Any] = {
            "search": {"type": "object", "properties": {"query": {"type": "string"}}},
            "zoom": {"type": "object", "properties": {"path": {"type": "string"}}},
        }

    def classify(self, head: dict) -> str:
        return derive_slice_class([self.path], schema_blobs=(json.dumps(self.base).encode(), json.dumps(head).encode()))

    def test_zoom_only_schema_change_is_non_ranking(self) -> None:
        head = copy.deepcopy(self.base)
        head["zoom"]["properties"] = {"batch": {"type": "array"}}
        self.assertEqual(self.classify(head), "non_ranking")

    def test_search_schema_change_stays_fenced(self) -> None:
        head = copy.deepcopy(self.base)
        head["search"]["properties"]["query"]["description"] = "New search guidance"
        self.assertEqual(self.classify(head), "ranking")

    def test_referenced_shared_definition_change_stays_fenced(self) -> None:
        self.base["$defs"] = {"Query": {"type": "string"}}
        self.base["search"]["properties"]["query"] = {"$ref": "#/$defs/Query"}
        head = copy.deepcopy(self.base)
        head["$defs"]["Query"]["minLength"] = 1
        self.assertEqual(self.classify(head), "ranking")

    def test_malformed_head_fails_closed_with_gate_diagnostic(self) -> None:
        manifest, reference, score = synthetic_documents()
        result = total_gate(reference, score, manifest, None, [self.path], schema_blobs=(json.dumps(self.base).encode(), b'{"search":'))
        self.assertEqual(result.exit_code, 1)
        self.assertIn("missing ranking descriptor", result.reasons)
        self.assertIn(f"ranking_fence:{self.path}:fail_closed:head:invalid_json", result.notes)

    def test_non_search_schema_and_parity_changes_need_no_engine_unwired_descriptor(self) -> None:
        head = copy.deepcopy(self.base)
        head["zoom"]["description"] = "Batch-only zoom"
        manifest, reference, score = synthetic_documents()
        descriptor = {"slice_class": "non_ranking", "targeted_mechanism": "none", "kind": "harness", "fixtures": ["harness-goldens"]}
        result = total_gate(reference, score, manifest, descriptor, [self.path, TOOL_CALL_PARITY_FIXTURE_SOURCE], schema_blobs=(json.dumps(self.base).encode(), json.dumps(head).encode()))
        self.assertEqual(result.exit_code, 0, result.reasons)

    def test_formatting_and_key_order_are_not_schema_changes(self) -> None:
        blobs = (json.dumps(self.base, indent=2).encode(), json.dumps(dict(reversed(list(self.base.items())))).encode())
        self.assertEqual(derive_slice_class([self.path], schema_blobs=blobs), "non_ranking")

    def test_shared_version_change_stays_fenced(self) -> None:
        self.base["version"] = 1
        head = copy.deepcopy(self.base)
        head["version"] = 2
        self.assertEqual(self.classify(head), "ranking")

    def test_json_comparison_distinguishes_booleans_from_numbers_not_numeric_formatting(self) -> None:
        self.base["version"] = 1
        head = copy.deepcopy(self.base)
        head["version"] = 1.0
        self.assertEqual(self.classify(head), "non_ranking")
        head["version"] = True
        self.assertEqual(self.classify(head), "ranking")

    def test_json_numbers_do_not_lose_precision_during_comparison(self) -> None:
        blob = json.dumps(self.base).encode()[:-1] + b', "version": 1e400}'
        self.assertEqual(derive_slice_class([self.path], schema_blobs=(blob, blob.replace(b'1e400', b'2e400'))), "ranking")

    def test_reference_to_another_tool_keeps_that_dependency_fenced(self) -> None:
        self.base["search"]["properties"]["query"] = {"$ref": "#/zoom/properties/path"}
        head = copy.deepcopy(self.base)
        head["zoom"]["properties"]["path"]["minLength"] = 1
        self.assertEqual(self.classify(head), "ranking")

    def test_unknown_catalog_keys_are_not_silently_excluded(self) -> None:
        head = copy.deepcopy(self.base)
        head["new_tool_or_metadata"] = {"type": "object"}
        self.assertEqual(self.classify(head), "ranking")

    def test_other_ranking_files_still_fence_a_zoom_only_schema_change(self) -> None:
        blobs = (json.dumps(self.base).encode(), json.dumps(self.base).encode())
        self.assertEqual(derive_slice_class([self.path, "crates/aft/src/query_shape.rs"], schema_blobs=blobs), "ranking")

    def test_unknown_shapes_and_missing_blobs_fail_closed(self) -> None:
        valid = json.dumps(self.base).encode()
        for blob in (None, b'[]', b'{"tools":{"search":{}}}', b'{"search":{}}', b'{"search":true}', b'{"search":{},"search":{}}', b'NaN', b'\xff'):
            for side in (0, 1):
                with self.subTest(blob=blob, side=side):
                    blobs = (blob, valid) if side == 0 else (valid, blob)
                    notes = []
                    self.assertEqual(derive_slice_class([self.path], schema_blobs=blobs, notes=notes), "ranking")
                    self.assertIn("fail_closed:", notes[0])
        notes = []
        self.assertEqual(derive_slice_class([self.path], notes=notes), "ranking")
        self.assertIn("comparison_unavailable", notes[0])

    def test_unresolved_or_unsupported_references_fail_closed(self) -> None:
        for ref in ("#/$defs/Missing", "https://example.test/query.json", "#anchor", "#/bad~2escape"):
            with self.subTest(ref=ref):
                self.base["search"]["properties"]["query"] = {"$ref": ref}
                notes = []
                blob = json.dumps(self.base).encode()
                self.assertEqual(derive_slice_class([self.path], schema_blobs=(blob, blob), notes=notes), "ranking")
                self.assertIn("schema_reference", notes[0])

    def test_fail_closed_note_survives_descriptor_class_mismatch(self) -> None:
        manifest, reference, score = synthetic_documents()
        descriptor = {"slice_class": "non_ranking", "targeted_mechanism": "none", "kind": "harness", "fixtures": ["harness-goldens"]}
        result = total_gate(reference, score, manifest, descriptor, [self.path], schema_blobs=(b'broken', b'broken'))
        self.assertEqual(result.exit_code, 2)
        self.assertIn("descriptor_class_mismatch", result.reasons[0])
        self.assertIn("fail_closed:base:invalid_json", result.notes[0])

    def test_blob_reader_uses_merge_base_and_explicit_head_not_worktree(self) -> None:
        with mock.patch("search_quality_lib.subprocess.run") as git:
            git.side_effect = [mock.Mock(returncode=0, stdout=b"merge-base-sha\n"), mock.Mock(returncode=0, stdout=b"base"), mock.Mock(returncode=0, stdout=b"head")]
            root = Path("/repo")
            self.assertEqual(read_tool_schema_blobs("base-ref", "head-ref", [self.path], root=root), (b"base", b"head"))
            self.assertEqual([call.args[0] for call in git.call_args_list], [
                ["git", "merge-base", "base-ref", "head-ref"],
                ["git", "show", f"merge-base-sha:{self.path}"],
                ["git", "show", f"head-ref:{self.path}"],
            ])
            self.assertTrue(all(call.kwargs["cwd"] == root for call in git.call_args_list))

    def test_blob_reader_leaves_missing_head_for_fail_closed_classification(self) -> None:
        with mock.patch("search_quality_lib.subprocess.run") as git:
            git.side_effect = [mock.Mock(returncode=0, stdout=b"base-sha\n"), mock.Mock(returncode=0, stdout=b"base"), mock.Mock(returncode=1, stdout=b"")]
            self.assertEqual(read_tool_schema_blobs("base", "head", [self.path], root=Path("/repo")), (b"base", None))
        with mock.patch("search_quality_lib.subprocess.run") as git:
            self.assertIsNone(read_tool_schema_blobs("base", "head", ["README.md"], root=Path("/repo")))
            git.assert_not_called()

    def test_diff_paths_keeps_deleted_schema_path_visible_even_for_a_rename(self) -> None:
        with mock.patch("search_quality.subprocess.run", return_value=mock.Mock(returncode=0, stdout=self.path + "\n")) as git:
            self.assertEqual(diff_paths("base", "head"), [self.path])
        self.assertEqual(git.call_args.args[0], ["git", "diff", "--name-only", "--no-renames", "base...head"])

    def test_evaluate_cli_supplies_committed_schema_blobs_to_gate(self) -> None:
        manifest, reference, score = synthetic_documents()
        head = copy.deepcopy(self.base)
        head["zoom"]["description"] = "Batch-only zoom"
        blobs = (json.dumps(self.base).encode(), json.dumps(head).encode())
        with tempfile.TemporaryDirectory() as directory:
            score_path = Path(directory) / "score.json"
            args = quality_parser().parse_args(["--score", str(score_path), "--base-ref", "base-ref", "--head", "head-ref"])
            with mock.patch("search_quality.read_json", side_effect=[manifest, score, reference]), mock.patch("search_quality.binding"), mock.patch("search_quality.descriptor_path", return_value=None), mock.patch("search_quality.diff_paths", return_value=[self.path]), mock.patch("search_quality.read_tool_schema_blobs", return_value=blobs) as reader:
                self.assertEqual(quality_run(args), 0)
            reader.assert_called_once_with("base-ref", "head-ref", [self.path], root=Path(__file__).resolve().parents[2])


class EngineUnwiredGateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.manifest, self.reference, self.score = synthetic_documents()
        self.descriptor = {
            "slice_class": "engine_unwired",
            "targeted_mechanism": "none",
            "kind": "harness",
            "fixtures": [TOOL_CALL_PARITY_FIXTURE_SOURCE],
        }
        self.ranking_paths = ["crates/aft/src/commands/semantic_search/scoring.rs"]

    def gate(self, score: dict, paths: list[str] | None = None):
        return total_gate(
            self.reference,
            score,
            self.manifest,
            self.descriptor,
            self.ranking_paths if paths is None else paths,
        )

    def test_engine_unwired_accepts_byte_equal_ranking_results(self) -> None:
        self.assertEqual(self.gate(self.score).exit_code, 0)

    def test_plugin_paths_outside_the_search_tools_are_non_ranking(self) -> None:
        from search_quality_lib import derive_slice_class

        self.assertEqual(
            derive_slice_class(
                ["packages/opencode-plugin/src/tools/bash_watch.ts", "packages/pi-plugin/src/tools/bash.ts"]
            ),
            "non_ranking",
        )
        self.assertEqual(
            derive_slice_class(["packages/opencode-plugin/src/tools/semantic.ts"]), "ranking"
        )
        self.assertEqual(
            derive_slice_class(["packages/pi-plugin/src/__tests__/semantic.test.ts"]), "ranking"
        )

    def test_search_router_paths_are_ranking(self) -> None:
        from search_quality_lib import derive_slice_class

        # The router chooses which lanes run for a query, so a router-only
        # diff must face the ranking gate rather than pass as non-ranking.
        self.assertEqual(derive_slice_class(["crates/aft/src/search_b2/router.rs"]), "ranking")
        self.assertEqual(derive_slice_class(["crates/aft/src/search_b2/lane_plan.rs"]), "ranking")

    def test_parser_paths_are_ranking(self) -> None:
        from search_quality_lib import derive_slice_class

        # parser.rs decides symbol ranges, which become semantic chunk
        # boundaries, so a parser-only diff can move ranked rows.
        self.assertEqual(derive_slice_class(["crates/aft/src/parser.rs"]), "ranking")
        # symbols.rs only defines the Symbol and Range types, so it stays out.
        self.assertEqual(derive_slice_class(["crates/aft/src/symbols.rs"]), "non_ranking")

    def test_engine_unwired_rejects_a_non_ranking_diff_class(self) -> None:
        result = self.gate(self.score, ["scripts/telemetry/cost-gate.sh"])
        self.assertEqual(result.exit_code, 2)
        self.assertEqual(
            result.reasons,
            ("descriptor_class_mismatch:declared=engine_unwired:derived=non_ranking",),
        )

    def test_engine_unwired_names_the_first_changed_real_query_row(self) -> None:
        changed = copy.deepcopy(self.score)
        changed["rows"][0]["ranked_paths"] = ["different.rs"]
        result = self.gate(changed)
        self.assertEqual(result.exit_code, 2)
        self.assertEqual(
            result.reasons,
            ("engine_unwired_mismatch:row=real_query.followup-census:1",),
        )

    def test_engine_unwired_accepts_cross_python_aggregate_rounding(self) -> None:
        self.reference["families"]["real_query"]["mrr_at_10"] = 0.1976190476190476
        self.reference["shapes"]["identifier"]["mrr_at_10"] = 0.1976190476190476
        self.reference["mechanisms"]["topk_cut"]["mrr_at_10"] = 0.1845238095238095
        self.reference["census_weighted_mrr_report_only"] = 0.042149841269841275
        changed = copy.deepcopy(self.reference)
        changed["families"]["real_query"]["mrr_at_10"] = 0.19761904761904764
        changed["shapes"]["identifier"]["mrr_at_10"] = 0.19761904761904764
        changed["mechanisms"]["topk_cut"]["mrr_at_10"] = 0.18452380952380953
        changed["census_weighted_mrr_report_only"] = 0.04214984126984127
        self.assertEqual(self.gate(changed).exit_code, 0)

    def test_engine_unwired_rejects_real_query_aggregate_drift(self) -> None:
        changed = copy.deepcopy(self.score)
        changed["families"]["real_query"]["mrr_at_10"] = 0.6
        result = self.gate(changed)
        self.assertEqual(result.exit_code, 2)
        self.assertEqual(
            result.reasons,
            ("engine_unwired_mismatch:row=real_query.families.real_query.mrr_at_10",),
        )

    def test_engine_unwired_rejects_exact_family_drift(self) -> None:
        changed = copy.deepcopy(self.score)
        changed["families"]["exact_recall"]["mrr_at_10"] = 0.4
        result = self.gate(changed)
        self.assertEqual(result.exit_code, 2)
        self.assertEqual(result.reasons, ("engine_unwired_mismatch:row=exact_recall.family",))

    def test_engine_unwired_rejects_concept_fixture_group_drift(self) -> None:
        changed = copy.deepcopy(self.score)
        changed["fixture_groups"]["concept_recall"]["g"]["hit_at_5"] = 0.7
        result = self.gate(changed)
        self.assertEqual(result.exit_code, 2)
        self.assertEqual(result.reasons, ("engine_unwired_mismatch:row=concept_recall.g",))

    def test_engine_unwired_labels_a_missing_family_row_as_mismatch(self) -> None:
        changed = copy.deepcopy(self.score)
        del changed["fixture_groups"]["concept_recall"]["g"]
        result = self.gate(changed)
        self.assertEqual(result.exit_code, 2)
        self.assertEqual(result.reasons, ("engine_unwired_mismatch:row=concept_recall.g",))

    def test_engine_unwired_labels_profile_drift_before_profile_validation(self) -> None:
        changed = copy.deepcopy(self.score)
        changed["profile"] = "paged"
        result = self.gate(changed)
        self.assertEqual(result.exit_code, 2)
        self.assertEqual(result.reasons, ("engine_unwired_mismatch:row=real_query.profile",))

    def test_engine_unwired_rejects_inline_parity_fixture_changes(self) -> None:
        result = self.gate(self.score, self.ranking_paths + [TOOL_CALL_PARITY_FIXTURE_SOURCE])
        self.assertEqual(result.exit_code, 2)
        self.assertEqual(
            result.reasons,
            (f"engine_unwired_mismatch:row=tool_call_parity.{TOOL_CALL_PARITY_FIXTURE_SOURCE}",),
        )

    def test_task_and_train_branches_resolve_the_train_descriptor_label(self) -> None:
        self.assertIn("train-55", descriptor_labels("train/55"))
        self.assertIn("train-55", descriptor_labels("alfonso/task/r48-engine-unwired-train-55-"))


class EngineUnwiredPresentationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.manifest, self.reference, self.score = synthetic_documents()
        self.episode_id = "followup-census:1"
        self.before = ["opened.rs [lexical match]", "other.rs [lexical match]"]
        self.after = ["opened.rs [lexical match]", "  opened [function] lines 1-3"]
        self.reference["rows"][0]["summary_text"] = list(self.before)
        self.score["rows"][0]["summary_text"] = list(self.after)
        self.descriptor = {
            "slice_class": "engine_unwired",
            "targeted_mechanism": "none",
            "kind": "harness",
            "fixtures": [TOOL_CALL_PARITY_FIXTURE_SOURCE],
            "presentation_rows": {self.episode_id: {"before": self.before, "after": self.after}},
        }
        self.paths = ["crates/aft/src/commands/semantic_search/mod.rs"]

    def gate(self):
        return total_gate(self.reference, self.score, self.manifest, self.descriptor, self.paths)

    def assert_mismatch(self, field: str) -> None:
        result = self.gate()
        self.assertEqual(result.exit_code, 2)
        self.assertEqual(result.reasons, (f"engine_unwired_mismatch:row=real_query.{field}",))

    def test_presentation_rows_accepts_only_exact_declared_transitions(self) -> None:
        for document in (self.reference, self.score):
            row = copy.deepcopy(document["rows"][0])
            row["episode_id"] = "followup-census:2"
            document["rows"].append(row)
        manifest_row = copy.deepcopy(self.manifest["rows"][0])
        manifest_row["episode_id"] = "followup-census:2"
        self.manifest["rows"].append(manifest_row)
        self.descriptor["presentation_rows"]["followup-census:2"] = copy.deepcopy(
            self.descriptor["presentation_rows"][self.episode_id]
        )
        self.assertEqual(self.gate().exit_code, 0)

        # The allowance identifies rows, but does not turn their order into a set.
        self.score["rows"].reverse()
        self.assert_mismatch(f"{self.episode_id}.episode_id")

    def test_presentation_rows_refuses_an_unlisted_summary_change(self) -> None:
        for document in (self.reference, self.score):
            row = copy.deepcopy(document["rows"][0])
            row["episode_id"] = "followup-census:2"
            row["summary_text"] = ["same"]
            document["rows"].append(row)
        manifest_row = copy.deepcopy(self.manifest["rows"][0])
        manifest_row["episode_id"] = "followup-census:2"
        self.manifest["rows"].append(manifest_row)
        self.score["rows"][1]["summary_text"] = ["changed"]
        self.assert_mismatch("followup-census:2.summary_text")

    def test_presentation_rows_refuses_wrong_before_lines(self) -> None:
        self.descriptor["presentation_rows"][self.episode_id]["before"] = ["wrong"]
        self.assert_mismatch(f"{self.episode_id}.summary_text:before_mismatch")

    def test_presentation_rows_refuses_wrong_after_lines(self) -> None:
        self.descriptor["presentation_rows"][self.episode_id]["after"] = ["wrong"]
        self.assert_mismatch(f"{self.episode_id}.summary_text:after_mismatch")

    def test_presentation_rows_refuses_a_listed_row_that_did_not_change(self) -> None:
        self.score["rows"][0]["summary_text"] = list(self.before)
        self.descriptor["presentation_rows"][self.episode_id]["after"] = list(self.before)
        self.assert_mismatch(f"{self.episode_id}.summary_text:unchanged")

    def test_presentation_rows_refuses_a_missing_listed_row_or_summary(self) -> None:
        for side in ("reference", "score"):
            for missing in ("row", "summary_text"):
                with self.subTest(side=side, missing=missing):
                    self.setUp()
                    document = getattr(self, side)
                    if missing == "row":
                        document["rows"].clear()
                        reason = f"missing_{side}_row"
                    else:
                        del document["rows"][0]["summary_text"]
                        reason = "before_mismatch" if side == "reference" else "after_mismatch"
                    self.assert_mismatch(f"{self.episode_id}.summary_text:{reason}")

    def test_presentation_rows_keeps_every_other_row_field_strict(self) -> None:
        for field, value in (
            ("ranked_paths", ["different.rs"]),
            ("pattern_summary", "changed"),
            ("metrics", {"mrr_at_10": 0.6}),
            ("retrieval_depth", 2),
            ("request_count", 2),
        ):
            with self.subTest(field=field):
                self.setUp()
                self.score["rows"][0][field] = value
                self.assert_mismatch(f"{self.episode_id}.{field}")

    def test_presentation_rows_keeps_families_and_aggregates_strict(self) -> None:
        for family in ("exact_recall", "concept_recall", "real_query"):
            with self.subTest(family=family):
                self.setUp()
                self.score["families"][family]["mrr_at_10"] = 0.6
                result = self.gate()
                self.assertEqual(result.exit_code, 2)
                field = f"real_query.families.{family}.mrr_at_10" if family == "real_query" else f"{family}.family"
                self.assertEqual(result.reasons, (f"engine_unwired_mismatch:row={field}",))

    def test_presentation_rows_is_engine_unwired_only(self) -> None:
        for slice_class, kind, paths in (
            ("ranking", "paging", self.paths),
            ("non_ranking", "harness", ["scripts/telemetry/cost-gate.sh"]),
        ):
            with self.subTest(slice_class=slice_class):
                descriptor = dict(self.descriptor, slice_class=slice_class, kind=kind)
                with self.assertRaisesRegex(InputFault, "malformed_descriptor:presentation_rows:engine_unwired_only"):
                    resolve_descriptor(descriptor, paths)

    def test_presentation_rows_validator_refuses_malformed_entries(self) -> None:
        for entries in (
            [],
            {"invalid-id": {"before": [], "after": []}},
            {self.episode_id: []},
            {self.episode_id: {"before": self.before}},
            {self.episode_id: {"before": self.before, "after": self.after, "ranked_paths": []}},
            {self.episode_id: {"before": "not lines", "after": self.after}},
            {self.episode_id: {"before": self.before, "after": [1]}},
        ):
            with self.subTest(entries=entries):
                descriptor = dict(self.descriptor, presentation_rows=entries)
                with self.assertRaises(InputFault):
                    resolve_descriptor(descriptor, self.paths)


class PageInvarianceExcuseGateTests(unittest.TestCase):
    """A manifest's page-invariance excuse covers the reference, never a changed engine."""

    RANKING = (
        {"slice_class": "ranking", "targeted_mechanism": "none", "kind": "paging", "fixtures": ["paging"]},
        ["crates/aft/src/commands/semantic_search/memo.rs"],
    )
    ENGINE_UNWIRED = (
        {
            "slice_class": "engine_unwired",
            "targeted_mechanism": "none",
            "kind": "harness",
            "fixtures": [TOOL_CALL_PARITY_FIXTURE_SOURCE],
        },
        ["crates/aft/src/commands/semantic_search/memo.rs"],
    )
    NON_RANKING = (None, [])

    def setUp(self) -> None:
        self.manifest, self.reference, self.score = synthetic_documents()
        self.manifest["rows"][0][REFERENCE_NOT_PAGE_INVARIANT_FIELD] = "defect under measurement"
        self.episode_id = self.score["rows"][0]["episode_id"]
        self.failure = f"page_invariance_failed:{self.episode_id}"

    def record_miss(self, document: dict) -> dict:
        changed = copy.deepcopy(document)
        changed["rows"][0][PAGE_INVARIANCE_FAILED_FIELD] = self.failure
        changed["rows"][0]["metrics"] = {"mrr_at_10": 0.0, "hit_at_1": 0.0, "hit_at_5": 0.0}
        return changed

    def gate(self, reference: dict, score: dict, slice_: tuple) -> tuple:
        descriptor, paths = slice_
        result = total_gate(reference, score, self.manifest, descriptor, paths)
        return result.exit_code, result.reasons

    def test_ranking_slice_faults_on_a_flagged_row_that_breaks_page_invariance(self) -> None:
        reference = self.record_miss(self.reference)
        self.assertEqual(self.gate(reference, self.score, self.RANKING)[0], 2)  # rows differ: latency-only
        # Even a byte-exact reproduction of the reference's miss is refused.
        self.assertEqual(
            self.gate(reference, self.record_miss(self.score), self.RANKING),
            (2, (f"{self.failure}:candidate",)),
        )

    def test_unchanged_engine_slice_accepts_a_reproduced_reference_miss(self) -> None:
        reference = self.record_miss(self.reference)
        candidate = self.record_miss(self.score)
        self.assertEqual(self.gate(reference, candidate, self.NON_RANKING), (0, ()))
        self.assertEqual(self.gate(reference, candidate, self.ENGINE_UNWIRED), (0, ()))

    def test_unchanged_engine_slice_faults_when_the_miss_is_not_reproduced(self) -> None:
        fault = (2, (f"page_invariance_failed:{self.episode_id}:reference_miss_not_reproduced",))
        reference_miss = self.record_miss(self.reference)
        candidate_miss = self.record_miss(self.score)
        # A row the reference scored normally now fails invariance.
        self.assertEqual(self.gate(self.reference, candidate_miss, self.NON_RANKING), fault)
        # The reference's miss is not reproduced: the row is invariant now.
        self.assertEqual(self.gate(reference_miss, self.score, self.NON_RANKING), fault)
        # The failure record differs.
        other = copy.deepcopy(candidate_miss)
        other["rows"][0][PAGE_INVARIANCE_FAILED_FIELD] = f"{self.failure}:scoring"
        self.assertEqual(self.gate(reference_miss, other, self.NON_RANKING), fault)
        # A failing row that still scores is not a miss.
        scored = copy.deepcopy(candidate_miss)
        scored["rows"][0]["metrics"]["mrr_at_10"] = 1.0
        self.assertEqual(self.gate(reference_miss, scored, self.NON_RANKING), fault)
        self.assertEqual(self.gate(self.reference, candidate_miss, self.ENGINE_UNWIRED), fault)

    def test_invariance_failure_on_an_unflagged_row_is_refused_by_population_check(self) -> None:
        self.manifest["rows"][0].pop(REFERENCE_NOT_PAGE_INVARIANT_FIELD)
        with self.assertRaisesRegex(InputFault, "not_excused_by_manifest"):
            validate_scored_population(self.manifest, self.record_miss(self.score))


class LatencyOnlyRankingGateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.manifest, self.reference, self.score = synthetic_documents()
        self.descriptor = {
            "slice_class": "ranking",
            "targeted_mechanism": "none",
            "kind": "paging",
            "fixtures": ["repeated-paged-exact-memo"],
        }
        self.paths = ["crates/aft/src/commands/semantic_search/memo.rs"]

    def test_latency_only_ranking_accepts_byte_equal_results(self) -> None:
        result = total_gate(
            self.reference, self.score, self.manifest, self.descriptor, self.paths
        )
        self.assertEqual(result.exit_code, 0)

    def test_latency_only_ranking_rejects_row_drift(self) -> None:
        changed = copy.deepcopy(self.score)
        changed["rows"][0]["ranked_paths"] = ["different.rs"]
        result = total_gate(
            self.reference, changed, self.manifest, self.descriptor, self.paths
        )
        self.assertEqual(result.exit_code, 2)
        self.assertEqual(
            result.reasons,
            ("latency_only_ranking_mismatch:row=real_query.followup-census:1",),
        )


class UnreachableSplitRowTests(unittest.TestCase):
    """The `unreachable_split_rows` waiver for absolute split predicates."""

    STALE = "followup-census:910001"
    ANSWER = "crates/aft/src/commands/ast_search.rs"

    def documents(self, split_paths, prose_paths, *, kind="R1", answer_kind="concept", pattern="ast_grep_search"):
        """A ranking evaluation with one split row whose answer is ANSWER."""
        _, reference, score = synthetic_documents()
        manifest = {"rows": [{"episode_id": self.STALE, "pattern": pattern, "opened_file": self.ANSWER,
                              "split_kind": kind, "answer_kind": answer_kind}]}
        row = {"episode_id": self.STALE, "input_form": "split", "answer_kind": answer_kind, "split_kind": kind,
               "ranked_paths": split_paths, "metrics": row_metrics(split_paths, self.ANSWER),
               "prose_only": {"ranked_paths": prose_paths, "metrics": row_metrics(prose_paths, self.ANSWER)}}
        score["rows"] = [row]
        score["mechanisms"]["topk_cut"]["mrr_at_10"] = 0.6
        return reference, score, manifest

    def descriptor(self, predicate="split_rank1_required", episode_id=None):
        return {"slice_class": "ranking", "targeted_mechanism": "topk_cut", "kind": "ranking",
                UNREACHABLE_SPLIT_ROWS_FIELD: [{"episode_id": episode_id or self.STALE, "predicate": predicate,
                                                 "reason": "stale name; prose never ranks the answer"}]}

    def judge(self, reference, score, manifest, descriptor, answer_text="fn handle(req) {}\n"):
        failures = evaluate_predicate(reference, score, descriptor)
        return apply_unreachable_split_rows(failures, score, manifest, descriptor, lambda path: answer_text)

    def test_a_verified_unreachable_row_is_waived_and_printed(self) -> None:
        paths = ["other.rs", "another.rs"]
        reference, score, manifest = self.documents(paths, paths)
        self.assertIn(f"split_rank1_required:{self.STALE}", evaluate_predicate(reference, score, self.descriptor()))
        remaining, notes = self.judge(reference, score, manifest, self.descriptor())
        self.assertEqual(remaining, [])
        self.assertEqual(notes, [f"unreachable_split_row_waived:{self.STALE}:split_rank1_required:"
                                 "split_rank=-:prose_rank=-:pattern_in_answer_file=none:"
                                 "reason=stale name; prose never ranks the answer"])

    def test_a_waiver_is_refused_when_the_pattern_moved_the_answer(self) -> None:
        reference, score, manifest = self.documents(["other.rs", self.ANSWER], ["other.rs", "x.rs", self.ANSWER])
        remaining, notes = self.judge(reference, score, manifest, self.descriptor())
        self.assertIn(f"split_rank1_required:{self.STALE}", remaining)
        self.assertIn(f"unreachable_split_row_refused:{self.STALE}:split_rank1_required:"
                      "ranks_differ:split_rank=2:prose_rank=3", remaining)
        self.assertEqual(notes, [])

    def test_a_waiver_is_refused_when_the_pattern_matches_the_answer_file(self) -> None:
        paths = ["other.rs", self.ANSWER]
        reference, score, manifest = self.documents(paths, paths)
        remaining, _ = self.judge(reference, score, manifest, self.descriptor(),
                                  answer_text="use x;\n// see ast_grep_search\n")
        self.assertIn(f"split_rank1_required:{self.STALE}", remaining)
        self.assertIn(f"unreachable_split_row_refused:{self.STALE}:split_rank1_required:"
                      f"pattern_matches_answer_file:{self.ANSWER}:2", remaining)

    def test_a_waiver_is_refused_when_the_answer_file_cannot_be_read(self) -> None:
        paths = ["other.rs"]
        reference, score, manifest = self.documents(paths, paths)
        remaining, _ = self.judge(reference, score, manifest, self.descriptor(), answer_text=None)
        self.assertIn(f"unreachable_split_row_refused:{self.STALE}:split_rank1_required:"
                      f"answer_file_unreadable_at_pin:{self.ANSWER}", remaining)

    def test_paired_harm_can_never_be_waived(self) -> None:
        with self.assertRaisesRegex(InputFault, "predicate_not_waivable:split_paired_harm"):
            validate_unreachable_split_rows(self.descriptor(predicate="split_paired_harm"))
        # Even with a valid waiver for the row's absolute predicate, harm stands.
        reference, score, manifest = self.documents(["x.rs", "y.rs", self.ANSWER], [self.ANSWER])
        remaining, _ = self.judge(reference, score, manifest, self.descriptor())
        self.assertTrue(any(item.startswith(f"split_paired_harm:{self.STALE}") for item in remaining), remaining)

    def test_the_validator_refuses_malformed_entries(self) -> None:
        base = self.descriptor()
        cases = {
            "not_a_nonempty_list": [],
            "keys_must_be_episode_id_predicate_reason": [{"episode_id": self.STALE, "predicate": "split_rank1_required"}],
            "invalid_episode_id": [{"episode_id": "910001", "predicate": "split_rank1_required", "reason": "r"}],
            "predicate_not_waivable:shape": [{"episode_id": self.STALE, "predicate": "shape", "reason": "r"}],
            ":reason": [{"episode_id": self.STALE, "predicate": "split_rank1_required", "reason": "  "}],
            "duplicate": base[UNREACHABLE_SPLIT_ROWS_FIELD] * 2,
        }
        for expected, entries in cases.items():
            descriptor = dict(base, **{UNREACHABLE_SPLIT_ROWS_FIELD: entries})
            with self.assertRaisesRegex(InputFault, expected, msg=expected):
                validate_unreachable_split_rows(descriptor)
        for slice_class in ("non_ranking", "engine_unwired"):
            descriptor = dict(base, slice_class=slice_class)
            with self.assertRaisesRegex(InputFault, "ranking_only"):
                validate_unreachable_split_rows(descriptor)

    def test_a_waiver_naming_the_wrong_kind_or_a_non_split_row_is_refused(self) -> None:
        paths = ["other.rs"]
        reference, score, manifest = self.documents(paths, paths, kind="R4")
        with self.assertRaisesRegex(InputFault, "predicate_does_not_judge_row"):
            self.judge(reference, score, manifest, self.descriptor())
        with self.assertRaisesRegex(InputFault, "not_a_split_row:followup-census:1"):
            self.judge(reference, score, manifest, self.descriptor(episode_id="followup-census:1"))


if __name__ == "__main__":
    unittest.main()
