#!/usr/bin/env python3
"""PR descriptor validation uses real Git diffs without benchmark artifacts."""

from __future__ import annotations

import contextlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "benchmarks/aft-search"))
import search_quality


class PRSearchDescriptorTests(unittest.TestCase):
    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.repo = Path(directory.name)
        self.git("init", "-q")
        self.git("config", "user.email", "fixture@example.com")
        self.git("config", "user.name", "Fixture")
        self.commit_file("README.md", "base\n")
        self.base = self.git("rev-parse", "HEAD")
        self.branch = "feature/search"
        self.bench = self.repo / "benchmarks/aft-search"
        for name, value in (("ROOT", self.repo), ("BENCH", self.bench)):
            patcher = patch.object(search_quality, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def git(self, *args: str) -> str:
        return subprocess.check_output(["git", *args], cwd=self.repo, text=True).strip()

    def commit_file(self, path: str, text: str) -> None:
        file = self.repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)
        self.git("add", path)
        self.git("-c", "core.hooksPath=/dev/null", "commit", "-qm", "fixture")

    def descriptor(self, slice_class: str = "ranking") -> None:
        self.commit_file("benchmarks/aft-search/slice-descriptors/feature-search.json", json.dumps({
            "slice_class": slice_class,
            "targeted_mechanism": "wrong_lane_nl" if slice_class == "ranking" else "none",
            "kind": "ranking" if slice_class == "ranking" else "harness",
            "fixtures": ["harness-goldens"],
        }))

    def check(self, base: str | None = None) -> int:
        args = search_quality.parser().parse_args([
            "--mode", "check-descriptor", "--base-ref", self.base if base is None else base,
            "--head", self.git("rev-parse", "HEAD"), "--branch", self.branch,
        ])
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return search_quality.run(args)

    def test_non_ranking_diff_needs_no_descriptor_or_benchmark_artifacts(self) -> None:
        self.commit_file("docs/guide.md", "docs\n")
        self.assertEqual(self.check(), 0)

    def test_ranking_diff_without_descriptor_fails(self) -> None:
        self.commit_file("crates/aft/src/search_b2/router.rs", "ranking\n")
        self.assertEqual(self.check(), 1)

    def test_ranking_descriptor_matches_normalized_pr_branch(self) -> None:
        self.commit_file("crates/aft/src/search_b2/router.rs", "ranking\n")
        self.descriptor()
        self.assertEqual(self.check(), 0)

    def test_descriptor_class_mismatch_is_an_input_fault(self) -> None:
        self.commit_file("crates/aft/src/search_b2/router.rs", "ranking\n")
        self.descriptor("non_ranking")
        with self.assertRaisesRegex(search_quality.InputFault, "descriptor_class_mismatch"):
            self.check()

    def test_unknown_base_is_an_input_fault(self) -> None:
        with self.assertRaisesRegex(search_quality.InputFault, "unresolvable_diff"):
            self.check("does-not-exist")

    def test_empty_base_is_an_input_fault(self) -> None:
        with self.assertRaisesRegex(search_quality.InputFault, "missing_diff_base"):
            self.check("")


if __name__ == "__main__":
    unittest.main()
