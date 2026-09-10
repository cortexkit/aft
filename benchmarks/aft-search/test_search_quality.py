#!/usr/bin/env python3
from __future__ import annotations

import copy
import unittest

from search_quality import descriptor_labels, synthetic_documents
from search_quality_lib import TOOL_CALL_PARITY_FIXTURE_SOURCE, total_gate


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


if __name__ == "__main__":
    unittest.main()
