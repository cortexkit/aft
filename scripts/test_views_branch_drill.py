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


if __name__ == "__main__":
    unittest.main()
