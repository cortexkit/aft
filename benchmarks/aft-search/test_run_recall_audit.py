"""Offline cases for run_recall_audit.py: stage classification and fixture shape."""
from __future__ import annotations

import json
import unittest
from pathlib import Path

import run_recall_audit as audit

HERE = Path(__file__).resolve().parent
# Resolved, as the runner resolves it: on macOS /tmp is a link to /private/tmp.
ROOT = Path("/tmp/project").resolve()


def response(results=(), **audit_fields):
    """A search reply carrying a recall audit, with the fields a test overrides."""
    recall = {
        "shape": "natural_language",
        "lanes_run": ["exact", "lexical", "semantic"],
        "semantic_ran": True,
        "lanes": {"exact": [], "lexical": [], "lexical_pool_size": 0, "semantic_chunks": [], "path_lookup": []},
        "canonical_list": [],
        "targets": {},
        "retrieval_depth": 200,
    }
    lanes = audit_fields.pop("lanes", {})
    recall["lanes"].update(lanes)
    recall.update(audit_fields)
    return {
        "results": list(results),
        "structuredContent": {"plan": {"shape": "natural_language", "lanes_run": recall["lanes_run"], "confidence": "high"}},
        "recall_audit": recall,
    }


def case(truths, line_level=True, no_answer=False):
    return {
        "id": "case",
        "source": "named",
        "case_class": "test",
        "repo": "aft-evidence",
        "query": "q",
        "include_tests": False,
        "expect_no_answer": no_answer,
        "line_level": line_level,
        "truths": truths,
        "backend": "local",
    }


TRUTH = {"file_path": "src/a.rs", "line_start": 10, "line_end": 20, "relevance": 2}


def chunk(rank, start, end, file="src/a.rs"):
    return {"rank": rank, "file": file, "name": "f", "kind": "function", "start_line": start, "end_line": end, "score": 0.5}


class StageClassificationTests(unittest.TestCase):
    def test_answer_lines_on_the_page_are_found(self) -> None:
        reply = response([{"file": f"{ROOT}/src/a.rs", "start_line": 12, "end_line": 14}])
        row = audit.analyse_case(case([TRUTH]), reply, ROOT)
        self.assertEqual(row["stage"], "found")
        self.assertTrue(row["answer_in_top5"])

    def test_file_shown_through_another_chunk_is_dedupe_when_the_lane_produced_the_answer_chunk(self) -> None:
        reply = response(
            [{"file": f"{ROOT}/src/a.rs", "start_line": 90, "end_line": 95}],
            lanes={"semantic_chunks": [chunk(1, 90, 95), chunk(7, 11, 19)]},
        )
        row = audit.analyse_case(case([TRUTH]), reply, ROOT)
        self.assertEqual(row["stage"], "dropped_by_dedupe")
        self.assertFalse(row["answer_in_top5"])
        self.assertTrue(row["file_in_top5"])

    def test_file_shown_through_another_span_without_the_answer_chunk(self) -> None:
        reply = response([{"file": f"{ROOT}/src/a.rs", "start_line": 90, "end_line": 95}])
        row = audit.analyse_case(case([TRUTH]), reply, ROOT)
        self.assertEqual(row["stage"], "file_shown_other_span")

    def test_file_in_the_ranked_list_below_the_page(self) -> None:
        reply = response(canonical_list=[{"file": "src/b.rs"}, {"file": "src/a.rs"}])
        row = audit.analyse_case(case([TRUTH]), reply, ROOT)
        self.assertEqual(row["stage"], "ranked_below_page")
        self.assertEqual(row["truths"][0]["ranks"]["ranked_list"], 2)

    def test_lane_candidate_past_the_block_depth_is_not_admitted(self) -> None:
        reply = response(lanes={"lexical": ["src/b.rs", "src/a.rs"]})
        row = audit.analyse_case(case([TRUTH]), reply, ROOT)
        self.assertEqual(row["stage"], "not_admitted")
        self.assertIn("block depth", row["truths"][0]["detail"])

    def test_lexical_score_outside_the_discovery_pool_is_not_admitted(self) -> None:
        reply = response(targets={"src/a.rs": {"trigram_index": "indexed", "lexical_unpooled_rank": 40, "semantic_chunks": []}})
        row = audit.analyse_case(case([TRUTH]), reply, ROOT)
        self.assertEqual(row["stage"], "not_admitted")
        self.assertIn("discovery pool", row["truths"][0]["detail"])

    def test_semantic_rank_only_counts_as_a_cut_when_semantic_ran(self) -> None:
        targets = {"src/a.rs": {"trigram_index": "indexed", "lexical_unpooled_rank": None, "semantic_chunks": [chunk(300, 11, 19)]}}
        ran = audit.analyse_case(case([TRUTH]), response(targets=targets), ROOT)
        self.assertEqual(ran["stage"], "not_admitted")
        not_run = audit.analyse_case(
            case([TRUTH]),
            response(targets=targets, semantic_ran=False, lanes_run=["exact", "lexical"], lanes={"semantic_chunks": None}),
            ROOT,
        )
        self.assertEqual(not_run["stage"], "not_produced")
        self.assertEqual(not_run["truths"][0]["ranks"]["semantic_store_chunk"], 300)

    def test_file_absent_from_both_stores_is_never_indexed(self) -> None:
        reply = response(targets={"src/a.rs": {"trigram_index": "absent", "lexical_unpooled_rank": None, "semantic_chunks": []}})
        row = audit.analyse_case(case([TRUTH]), reply, ROOT)
        self.assertEqual(row["stage"], "never_indexed")

    def test_best_answer_is_the_one_that_got_furthest(self) -> None:
        other = {"file_path": "src/b.rs", "line_start": 1, "line_end": 2, "relevance": 1}
        reply = response(
            [{"file": f"{ROOT}/src/b.rs", "start_line": 1, "end_line": 1}],
            targets={"src/a.rs": {"trigram_index": "absent", "semantic_chunks": []}},
        )
        row = audit.analyse_case(case([TRUTH, other]), reply, ROOT)
        self.assertEqual(row["stage"], "found")
        self.assertEqual(row["best_truth"], "src/b.rs:1-2")

    def test_no_answer_row_records_confidence_and_is_never_a_top_five_hit(self) -> None:
        reply = response([{"file": f"{ROOT}/src/a.rs", "start_line": 1, "end_line": 1}])
        row = audit.analyse_case(case([], no_answer=True), reply, ROOT)
        self.assertEqual(row["stage"], "no_answer_expected")
        self.assertEqual(row["plan"]["confidence"], "high")
        self.assertFalse(row["answer_in_top5"])

    def test_reply_without_an_audit_is_marked_not_guessed(self) -> None:
        reply = {"results": [], "structuredContent": {"plan": {}}, "interpreted_as": "regex"}
        row = audit.analyse_case(case([TRUTH]), reply, ROOT)
        self.assertEqual(row["stage"], "audit_missing")


class NamedFixtureTests(unittest.TestCase):
    def test_every_named_row_states_an_answer_or_a_verified_no_answer(self) -> None:
        cases = audit.fixture_cases(HERE / "named-case-fixtures.json", "named", "case_class")
        payload = json.loads((HERE / "named-case-fixtures.json").read_text())
        self.assertEqual({task["case_class"] for task in payload["tasks"]}, set(payload["case_classes"]))
        for item in cases:
            self.assertTrue(item["verification"], item["id"])
            self.assertEqual(item["expect_no_answer"], not item["truths"], item["id"])

    def test_named_fixtures_are_kept_out_of_the_benchmark_index(self) -> None:
        ignored = (HERE / ".aftignore").read_text(encoding="utf-8").splitlines()
        self.assertIn("named-case-fixtures.json", ignored)


if __name__ == "__main__":
    unittest.main()
