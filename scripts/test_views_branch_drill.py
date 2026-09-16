#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import re
import sys
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path

MODULE_PATH = Path(__file__).parent / "lib" / "views-soak" / "branch_drill.py"
sys.path.insert(0, str(MODULE_PATH.parent))
SPEC = importlib.util.spec_from_file_location("views_branch_drill", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
import common


class ViewsBranchDrillProbeTest(unittest.TestCase):
    def test_probe_requires_exact_token_and_reference_evidence(self) -> None:
        query = MODULE.search_query_for_token("activeInfo")
        self.assertIsNotNone(re.search(query, "activeInfo(value)"))
        self.assertIsNone(re.search(query, "inactiveInformation"))
        self.assertFalse(MODULE.probe_has_definition_and_reference_evidence(1))
        self.assertTrue(MODULE.probe_has_definition_and_reference_evidence(2))

    def test_fresh_storage_guard_rejects_reused_arm_state(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            storage = Path(directory) / "arm"
            MODULE.assert_fresh_storage(storage, "arm storage")
            storage.mkdir()
            MODULE.assert_fresh_storage(storage, "arm storage")
            (storage / "manifest.json").write_text("{}", encoding="utf-8")
            with self.assertRaisesRegex(MODULE.SoakError, "must be fresh and empty"):
                MODULE.assert_fresh_storage(storage, "arm storage")

    def test_root_owned_phase_puts_overrides_membership_and_legacy_counters(self) -> None:
        root = Path("/tmp/opencode root")
        text = "\n".join([
            f"index_event kind=view_publication plane=views root={root} outcome=pending candidates=7045 blob_puts=8 pending_paths=260",
            f"index_event kind=view_publication plane=views root={root} outcome=published candidates=7045 blob_puts=0 pending_paths=0",
            f"content-addressed view publication published=true blob_puts=15 pending_paths=0 root={root}",
            f"index_event kind=view_publication plane=views root={root}-other outcome=published candidates=9000 blob_puts=99 pending_paths=0",
        ])
        _, puts, embeds, files = MODULE.log_metrics(text, root)
        self.assertEqual(puts, 0)
        self.assertEqual((embeds, files), (0, 0))

    def test_phase_puts_accumulate_across_graph_and_semantic_publications(self) -> None:
        root = Path("/tmp/opencode")
        text = "\n".join([
            f"index_event kind=view_publication plane=views root={root} outcome=published candidates=7045 blob_puts=269 pending_paths=260",
            f"index_event kind=view_publication plane=views root={root} outcome=published candidates=7045 blob_puts=13 pending_paths=0",
            f"index_event kind=view_publication plane=views root={root} outcome=no_op candidates=7045 blob_puts=0 pending_paths=0",
            f"content-addressed view publication published=true blob_puts=13 pending_paths=0 root={root}",
        ])
        self.assertEqual(MODULE.log_metrics(text, root)[1], 282)

    def test_cold_gate_retries_only_inspect_timeout_and_requires_completion(self) -> None:
        class Client:
            calls = 0
            def status(self, **kwargs):
                return {}
            def tool(self, *args, **kwargs):
                self.calls += 1
                return {"success": False, "text": "inspect_request_timeout"}
        client = Client()
        with patch.object(common, "semantic_work_description", return_value=None), patch.object(common, "_dead_code_phase_logged", side_effect=[False, False, True]), patch.object(common, "inspect_cache_has_dead_code", return_value=False), patch.object(common.time, "sleep"):
            result = common.wait_cold_work_ready(client, root=Path("/tmp/root"), storage_root=Path("/tmp/storage"), log_reader=lambda: "", timeout_s=10)
        self.assertEqual(client.calls, 2)
        self.assertEqual(result["inspect_timeout_retry_count"], 2)
        self.assertEqual(len(result["inspect_timeout_retry_elapsed_ms"]), 2)
        self.assertEqual(result["tier2_dead_code"], "complete")

    def test_cold_gate_does_not_retry_other_inspect_errors(self) -> None:
        class Client:
            def status(self, **kwargs):
                return {}
            def tool(self, *args, **kwargs):
                return {"success": False, "text": "permission denied"}
        with patch.object(common, "semantic_work_description", return_value=None), patch.object(common, "_dead_code_phase_logged", return_value=False), patch.object(common, "inspect_cache_has_dead_code", return_value=False):
            with self.assertRaisesRegex(common.SoakError, "permission denied"):
                common.wait_cold_work_ready(Client(), root=Path("/tmp/root"), storage_root=Path("/tmp/storage"), log_reader=lambda: "", timeout_s=10)


if __name__ == "__main__":
    unittest.main()
