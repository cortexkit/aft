#!/usr/bin/env python3
"""Focused unit checks for the views-soak parsing and accounting helpers."""

from __future__ import annotations

import json
import sqlite3
import tempfile
import unittest
from pathlib import Path

from branch_drill import delta_metrics, log_metrics, search_is_correct, symbols_in_source
from common import (
    ProcessSample,
    canonical_output,
    current_generation,
    first_differing_line,
    manifest_entry_count,
    parse_cpu_time,
    strip_jsonc,
)


class CommonTests(unittest.TestCase):
    def test_strip_jsonc_preserves_comment_markers_in_strings_and_removes_trailing_commas(self) -> None:
        source = r'''{
          // leading comment
          "url": "https://example.test/a//b",
          "literal": "/* retained */",
          "views": {"enabled": true,},
        }'''
        self.assertEqual(
            json.loads(strip_jsonc(source)),
            {
                "url": "https://example.test/a//b",
                "literal": "/* retained */",
                "views": {"enabled": True},
            },
        )

    def test_pointer_and_manifest_are_read_without_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            view = Path(raw)
            with sqlite3.connect(view / "pointer.sqlite") as connection:
                connection.execute(
                    "CREATE TABLE pointer(singleton INTEGER PRIMARY KEY, generation TEXT NOT NULL)"
                )
                connection.execute("INSERT INTO pointer VALUES (1, '7-deadbeef')")
            (view / "manifest-7-deadbeef.json").write_text(
                json.dumps({"path_identity_version": 1, "entries": [{"rel_path": "a.py"}]}),
                encoding="utf-8",
            )
            self.assertEqual(current_generation(view), "7-deadbeef")
            self.assertEqual(manifest_entry_count(view), 1)

    def test_output_normalizes_both_checkout_prefixes(self) -> None:
        left = Path("/tmp/source-root")
        right = Path("/tmp/baseline-root")
        output = canonical_output(
            {
                "id": "ignored",
                "success": True,
                "text": f"{left}/a.py\n{right}/b.py",
                "elapsed_ms": 99,
            },
            [left, right],
        )
        self.assertNotIn("/tmp/", output)
        self.assertNotIn("elapsed_ms", output)
        self.assertIn("<root>/a.py", output)
        self.assertIn("<root>/b.py", output)

    def test_first_differing_line_names_eof(self) -> None:
        self.assertEqual(first_differing_line("a\nb", "a"), (2, "b", "<EOF>"))

    def test_process_time_and_delta_support_ps_formats(self) -> None:
        self.assertEqual(parse_cpu_time("1-02:03:04.5"), 93784.5)
        self.assertEqual(parse_cpu_time("03:04.5"), 184.5)
        self.assertEqual(
            delta_metrics(ProcessSample(10.0, 2048), ProcessSample(12.5, 3072)),
            (2.5, 1.0),
        )


class BranchDrillTests(unittest.TestCase):
    def test_log_metrics_are_scoped_to_the_measured_checkout(self) -> None:
        root = Path("/tmp/opencode")
        text = "\n".join(
            [
                "content-addressed view HEAD reuse 98/100 for /tmp/opencode",
                "content-addressed view HEAD reuse 0/50 for /tmp/other",
                "content-addressed view publication published=true blob_puts=2 pending_paths=0",
                'semantic embedder refresh: root="/tmp/opencode" reason="watcher batch" files=3 chunks=4 batches=2 backend=x',
                'semantic embedder refresh: root="/tmp/other" reason="watcher batch" files=3 chunks=4 batches=7 backend=x',
            ]
        )
        self.assertEqual(log_metrics(text, root), (2, 2, 2))

    def test_source_symbol_extraction_covers_drill_languages(self) -> None:
        source = "\n".join(
            [
                "export async function alphaProbe() {}",
                "const betaProbe = () => 1",
                "pub fn gamma_probe() {}",
                "async def delta_probe(): pass",
                "func epsilonProbe() {}",
            ]
        )
        self.assertEqual(
            symbols_in_source(source),
            ["alphaProbe", "gamma_probe", "delta_probe", "epsilonProbe"],
        )

    def test_search_correctness_requires_a_real_result_hit(self) -> None:
        self.assertTrue(
            search_is_correct(
                {"success": True, "status": "ready", "results": [{"name": "uniqueNeedle"}]},
                "uniqueNeedle",
            )
        )
        self.assertFalse(
            search_is_correct(
                {"success": True, "status": "ready", "results": [], "query": "uniqueNeedle"},
                "uniqueNeedle",
            )
        )


if __name__ == "__main__":
    unittest.main()
