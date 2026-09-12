#!/usr/bin/env python3
from __future__ import annotations

import argparse
import copy
import json
import tempfile
import unittest
from pathlib import Path

from run_real_query import load_capability
from run_search_quality import selected_profile
from search_quality import (
    descriptor_labels,
    page_zero_evaluation_projection,
    synthetic_documents,
)
from search_quality_lib import (
    InputFault,
    TOOL_CALL_PARITY_FIXTURE_SOURCE,
    included_manifest_ids,
    total_gate,
    validate_included_row_mechanisms,
    validate_manifest_relabels,
    validate_profile_score,
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
                    "request": {"topK": 100, "offset": 0},
                    "requests": [
                        {"topK": 100, "offset": 0},
                        {"topK": 100, "offset": 100},
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


if __name__ == "__main__":
    unittest.main()
