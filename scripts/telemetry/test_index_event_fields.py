#!/usr/bin/env python3
"""Read the same bounded long-root record asserted by the Rust formatter test."""

import csv
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch

SCRIPT_DIR = Path(__file__).resolve().parent


def load_script(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, SCRIPT_DIR / filename)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {filename}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


gate = load_script("cost_gate_index_event_test", "cost-gate.py")
census = load_script("census_index_event_test", "index-census.py")
matrix = load_script("matrix_index_event_test", "oss-matrix.py")
index_event_root_matches = census.index_event_root_matches

FIXTURE = SCRIPT_DIR / "fixtures" / "index-event-long-root.txt"


class IndexEventFieldsTests(unittest.TestCase):
    def test_cost_gate_reads_all_measurements_from_bounded_long_root_record(self):
        line = FIXTURE.read_text().strip()
        self.assertLessEqual(len(line.encode()), 300)
        events = gate.parse_index_events(str(FIXTURE))
        self.assertEqual(events["semantic.first_query.count"], 1)
        for field, expected in (
            ("elapsed_ms", 5678),
            ("ready_to_first_query_ms", 9012),
            ("completed", 345),
            ("total", 678),
        ):
            self.assertEqual(events[f"semantic.first_query.{field}.p50"], expected)
            self.assertEqual(events[f"semantic.first_query.{field}.max"], expected)

    def test_census_and_matrix_parse_the_bounded_record(self):
        line = FIXTURE.read_text().strip()
        fields = census.parse_index_event_fields(line)
        self.assertEqual(fields, matrix.index_event_fields(line))
        self.assertEqual(fields["ready_to_first_query_ms"], "9012")
        self.assertEqual(fields["root"], ".../f2f0351c/typescript-eslint/worker")

    def test_hashed_root_attribution_distinguishes_equal_suffixes(self):
        suffix = "/typescript-eslint/worker"
        root = "/" + "a" * (300 - 1 - len(suffix)) + suffix
        logged = ".../f2f0351c/typescript-eslint/worker"
        self.assertTrue(index_event_root_matches(logged, root))
        self.assertTrue(index_event_root_matches(".../f2f0351c/worker", root))
        self.assertFalse(index_event_root_matches(logged, root.replace("a", "b")))
        self.assertFalse(index_event_root_matches(".../f2f0351c/other", root))
        self.assertTrue(index_event_root_matches("C:/work_tree/repo", "C:\\work tree\\repo"))

    def test_census_assigns_hashed_roots_without_cross_worktree_attribution(self):
        suffix = "/typescript-eslint/worker"
        matching_root = "/" + "a" * (300 - 1 - len(suffix)) + suffix
        sibling_root = "/" + "b" * (300 - 1 - len(suffix)) + suffix
        ambiguous_roots = (
            "/work/ambiguous_name" + suffix,
            "/work/ambiguous name" + suffix,
        )
        roots = (matching_root, sibling_root, *ambiguous_roots)
        line = FIXTURE.read_text().strip()
        # These two cache roots sanitize identically, so their abbreviated event
        # must remain unassigned rather than being credited to either project.
        ambiguous_line = line.replace(
            "root=.../f2f0351c/", "root=.../13bdb806/"
        )

        def empty_shape(root, record, repo):
            return census.RootInfo(
                root=root,
                key=record["key"],
                repo=repo,
                kind="worktree",
                git=False,
                exists=False,
                file_count=None,
                top_languages="n/a",
                workspace_shape="n/a",
            )

        with tempfile.TemporaryDirectory() as directory:
            scratch = Path(directory)
            logs = scratch / "logs"
            logs.mkdir()
            (logs / "aft-assignment.log").write_text(line + "\n" + ambiguous_line + "\n")
            cache_keys = scratch / "cache-keys.json"
            cache_keys.write_text(json.dumps({root: {"key": f"key-{i}"} for i, root in enumerate(roots)}))
            output = scratch / "output"
            with (
                patch.object(sys, "argv", ["index-census.py", str(logs), str(cache_keys)]),
                patch.dict(census.os.environ, {"CENSUS_OUTPUT_DIR": str(output)}),
                patch.object(census, "collect_shape", side_effect=empty_shape),
                redirect_stdout(io.StringIO()),
            ):
                self.assertEqual(census.main(), 0)
            with (output / "census-roots.csv").open() as handle:
                rows = {row["root"]: row for row in csv.DictReader(handle)}

        metric = "index_semantic_ready_to_first_query_ms_n_p50_max"
        self.assertEqual(rows[matching_root][metric], "1/9012/9012")
        for root in (sibling_root, *ambiguous_roots):
            with self.subTest(root=root):
                self.assertEqual(rows[root][metric], "0/n/a/n/a")


if __name__ == "__main__":
    unittest.main()
