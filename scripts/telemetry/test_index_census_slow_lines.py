#!/usr/bin/env python3
"""The index census must parse slow tool-call lines from old and new daemons."""

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent


def load_census():
    spec = importlib.util.spec_from_file_location("index_census_test", SCRIPT_DIR / "index-census.py")
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load index-census.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules["index_census_test"] = module
    spec.loader.exec_module(module)
    return module


census = load_census()

TAIL = "translate=0 exec=12000 format=0 finalize=0 egress=1 root=/repo"


class SlowToolLineTest(unittest.TestCase):
    def assert_parses(self, line: str) -> None:
        match = census.SLOW_TOOL_RE.search(line)
        self.assertIsNotNone(match, line)
        self.assertEqual(match["tool"], "tool.catalog")
        self.assertEqual(match["total"], "12345")
        self.assertEqual(match["queue"], "300")
        self.assertEqual(match["root"], "/repo")

    def test_line_without_session(self) -> None:
        self.assert_parses(
            f"slow tool_call name=tool.catalog channel=7 corr=1 total=12345ms queue=300 {TAIL}"
        )

    def test_line_with_session(self) -> None:
        self.assert_parses(
            f"slow tool_call name=tool.catalog session=ses_abc channel=7 corr=1 total=12345ms queue=300 {TAIL}"
        )

    def test_line_with_empty_session(self) -> None:
        self.assert_parses(
            f"slow tool_call name=tool.catalog session= channel=7 corr=1 total=12345ms queue=300 {TAIL}"
        )

    def test_line_with_wall_and_cpu_phase_times(self) -> None:
        for cpu in ["?", "0.004ms"]:
            with self.subTest(cpu=cpu):
                self.assert_parses(
                    "slow tool_call name=tool.catalog session=ses_abc channel=7 corr=1 "
                    f"total=12345ms queue=300.123ms/cpu={cpu} "
                    "translate=39.123ms/cpu=0.004ms exec=12000.000ms/cpu=? "
                    "waiting_on=limiter root=/repo"
                )


if __name__ == "__main__":
    unittest.main()
