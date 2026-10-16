"""Keep the response comparison sensitive to findings, gaps, and text."""

import copy
import unittest

from scoped_parity import canonical, producer_barrier_ready


class ScopedParityTests(unittest.TestCase):
    def barrier_observations(self):
        graph = {"success": True, "symbol": "DataSource::initialize",
                 "file": "packages/typeorm/src/data-source/DataSource.ts", "scanned_files": 1}
        response = {"success": True, "scanner_state": {"tier2_trigger_reason": "pull"},
                    "summary": {category: {"count": 0} for category in
                                ("dead_code", "unused_exports", "duplicates", "cycles", "complexity")}}
        return graph, response

    def test_producer_barrier_accepts_published_graph_and_finished_refresh(self):
        graph, response = self.barrier_observations()
        response["summary"]["complexity"].update({"complete": False, "parse_errors": [{"file": "invalid.ts"}]})
        self.assertTrue(producer_barrier_ready(graph, response))
        response["scanner_state"]["tier2_trigger_reason"] = "configure_warm"
        self.assertTrue(producer_barrier_ready(graph, response))

    def test_producer_barrier_rejects_unknown_or_pending_observations(self):
        graph, response = self.barrier_observations()
        for trigger in (None, "unobserved"):
            with self.subTest(trigger=trigger):
                altered = copy.deepcopy(response)
                altered["scanner_state"]["tier2_trigger_reason"] = trigger
                self.assertFalse(producer_barrier_ready(graph, altered))
        self.assertFalse(producer_barrier_ready({"success": False, "code": "callgraph_building"}, response))
        self.assertFalse(producer_barrier_ready(graph, {}))
        for category in response["summary"]:
            for row in ({}, {"count": None}, {"count": 0, "status": "pending"},
                        {"count": 0, "unavailable": True}, {"count": 0, "building": {"state": "building"}}):
                with self.subTest(category=category, row=row):
                    altered = copy.deepcopy(response)
                    altered["summary"][category] = row
                    self.assertFalse(producer_barrier_ready(graph, altered))

    def test_parity_preserves_object_field_order(self):
        self.assertNotEqual(canonical({"first": 1, "second": 2}),
                            canonical({"second": 2, "first": 1}))

    def test_canonical_ignores_only_request_id_and_last_run(self):
        before = {"id": "one", "scanner_state": {"tier2_last_run": 1, "state": "ready"}, "complete": False}
        after = copy.deepcopy(before)
        after["id"] = "two"
        after["scanner_state"]["tier2_last_run"] = 2
        self.assertEqual(canonical(before), canonical(after))
        self.assertEqual(before["id"], "one", "normalizing must not mutate raw evidence")
        after["complete"] = True
        self.assertNotEqual(canonical(before), canonical(after))

    def test_parity_rejects_finding_changes(self):
        before = {"summary": {"dead_code": {"count": 1}}, "text": "one finding", "gaps": [{"reason": "unknown"}]}
        for key, value in [("summary", {"dead_code": {"count": 0}}), ("text", "clean"), ("gaps", [])]:
            with self.subTest(key=key):
                after = {**before, key: value}
                self.assertNotEqual(canonical(before), canonical(after))


if __name__ == "__main__":
    unittest.main()
