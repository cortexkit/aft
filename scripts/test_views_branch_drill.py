#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import re
import sys
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).parent / "lib" / "views-soak" / "branch_drill.py"
sys.path.insert(0, str(MODULE_PATH.parent))
SPEC = importlib.util.spec_from_file_location("views_branch_drill", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ViewsBranchDrillProbeTest(unittest.TestCase):
    def test_probe_requires_exact_token_and_reference_evidence(self) -> None:
        query = MODULE.search_query_for_token("activeInfo")
        self.assertIsNotNone(re.search(query, "activeInfo(value)"))
        self.assertIsNone(re.search(query, "inactiveInformation"))
        self.assertFalse(MODULE.probe_has_definition_and_reference_evidence(1))
        self.assertTrue(MODULE.probe_has_definition_and_reference_evidence(2))

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


if __name__ == "__main__":
    unittest.main()
