#!/usr/bin/env python3
"""Harness integrity: portable transport, result paths, and answer-key isolation.

These cases cover the defects that made the search-quality gate unrunnable on
Windows (#326) and the one that let the benchmark index its own answer key
(#325). They are pure-Python and need no aft binary, so they run everywhere the
harness does.
"""
from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
import select
import shlex
import sys
import tempfile
import unittest
from pathlib import Path
from types import ModuleType
from typing import List, Optional
from unittest import mock

from evidence_tree import evidence_tree_sha256
from run import (
    AftClient,
    UnmatchedReportError,
    index_entry_count,
    normalize_result_path,
    refuse_all_unmatched,
    strip_verbatim_prefix,
)
from run_real_query import (
    PLATFORM_UNSUPPORTED_EXIT,
    UnsupportedPlatform,
    assert_reference_platform,
    main as real_query_main,
    runtime_evidence_tree,
)
from search_quality_lib import canonical_json

HERE = Path(__file__).resolve().parent

# A stand-in for the aft binary: one NDJSON frame in, one out, plus the noise
# frames a real run interleaves (progress pushes and non-JSON log lines).
FAKE_AFT_SOURCE = """import json
import sys

while True:
    line = sys.stdin.readline()
    if not line:
        break
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    sys.stdout.write("this line is not a frame\\n")
    sys.stdout.write(json.dumps({"id": "push", "event": "progress"}) + "\\n")
    sys.stdout.write(json.dumps({"id": request["id"], "success": True, "echo": request["command"]}) + "\\n")
    sys.stdout.flush()
"""


def windows_select_failure(*_args: object, **_kwargs: object) -> None:
    """What Windows raises when select() is handed a pipe instead of a socket."""
    raise OSError(10038, "An operation was attempted on something that is not a socket")


def write_fake_aft(directory: Path) -> Path:
    """A runnable fake aft binary, launched the way the real one is."""
    script = directory / "fake_aft.py"
    script.write_text(FAKE_AFT_SOURCE, encoding="utf-8")
    if os.name == "nt":
        launcher = directory / "fake-aft.cmd"
        launcher.write_text(f'@echo off\r\n"{sys.executable}" "{script}" %*\r\n', encoding="utf-8")
        return launcher
    launcher = directory / "fake-aft"
    launcher.write_text(
        "#!/bin/sh\nexec {} {} \"$@\"\n".format(shlex.quote(sys.executable), shlex.quote(str(script))),
        encoding="utf-8",
    )
    launcher.chmod(0o755)
    return launcher


def load_fusion_quality() -> ModuleType:
    """Import `run-fusion-quality`, which has no importable file name."""
    loader = importlib.machinery.SourceFileLoader("run_fusion_quality", str(HERE / "run-fusion-quality"))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    assert spec is not None
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


def ignore_entries() -> List[str]:
    lines = (HERE / ".aftignore").read_text(encoding="utf-8").splitlines()
    return [line.strip() for line in lines if line.strip() and not line.strip().startswith("#")]


class NdjsonTransportTests(unittest.TestCase):
    """The NDJSON clients must never poll a pipe with select()."""

    def test_baseline_client_reads_a_response_without_select(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = write_fake_aft(root)
            with mock.patch.object(select, "select", windows_select_failure):
                client = AftClient(binary, root, 10.0, storage_dir=root / "storage")
                try:
                    response = client.call("version", timeout_secs=30.0)
                finally:
                    if client.proc.stdin is not None:
                        client.proc.stdin.close()
                    client.close()
                    if client.proc.stdout is not None:
                        client.proc.stdout.close()
        self.assertEqual(response.get("echo"), "version")
        self.assertEqual(response.get("id"), "1")

    def test_real_query_client_reads_a_response_without_select(self) -> None:
        from run_real_query import NdjsonClient

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = write_fake_aft(root)
            with mock.patch.object(select, "select", windows_select_failure):
                client = NdjsonClient(binary, root, root / "storage", root / "aft.stderr")
                try:
                    response = client.call("version", timeout=30.0)
                finally:
                    if client.proc.stdin is not None:
                        client.proc.stdin.close()
                    client.close()
                    if client.proc.stdout is not None:
                        client.proc.stdout.close()
        self.assertEqual(response.get("echo"), "version")

    def test_request_frames_are_newline_delimited_json(self) -> None:
        self.assertTrue(canonical_json({"id": "1"}).endswith(b"\n"))


class ResultPathTests(unittest.TestCase):
    """A Windows verbatim path has to reach the harness in comparable form."""

    def test_verbatim_prefix_is_stripped_from_a_result_path(self) -> None:
        self.assertEqual(strip_verbatim_prefix("\\\\?\\C:\\tmp\\aft\\GUIDE.md"), "C:\\tmp\\aft\\GUIDE.md")
        self.assertEqual(strip_verbatim_prefix("//?/C:/tmp/aft/GUIDE.md"), "C:/tmp/aft/GUIDE.md")

    def test_ordinary_paths_are_left_alone(self) -> None:
        self.assertEqual(strip_verbatim_prefix("crates/aft/src/lib.rs"), "crates/aft/src/lib.rs")
        self.assertEqual(strip_verbatim_prefix("/tmp/aft/GUIDE.md"), "/tmp/aft/GUIDE.md")

    def test_verbatim_result_path_matches_the_relative_expected_path(self) -> None:
        for raw in ("\\\\?\\C:\\tmp\\aft\\GUIDE.md", "//?/C:/tmp/aft/GUIDE.md"):
            with self.subTest(raw=raw):
                normalized = normalize_result_path(raw, Path("/tmp/aft"))
                # expected_top_files entries are relative, so a normalized path
                # that still carries the verbatim prefix can never compare equal.
                self.assertFalse(normalized.startswith("//?/"), normalized)
                self.assertFalse(normalized.startswith("\\\\?\\"), normalized)

    @unittest.skipUnless(os.name == "nt", "AFT only returns verbatim paths on Windows")
    def test_verbatim_result_path_is_relative_to_the_project_root_on_windows(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            project_root = Path(directory).resolve()
            target = project_root / "crates" / "aft" / "src" / "lib.rs"
            target.parent.mkdir(parents=True)
            target.write_text("fn main() {}\n", encoding="utf-8")
            raw = "\\\\?\\" + str(target)
            self.assertEqual(normalize_result_path(raw, project_root), "crates/aft/src/lib.rs")


class ReportRefusalTests(unittest.TestCase):
    """An all-unmatched report on a healthy index is refused, not written."""

    def test_all_unmatched_report_is_refused_on_a_non_empty_index(self) -> None:
        with self.assertRaisesRegex(UnmatchedReportError, "0 of 3 fixtures matched"):
            refuse_all_unmatched([None, None, None], 33747, "the aft-search baseline report")

    def test_report_is_allowed_when_a_fixture_matched(self) -> None:
        refuse_all_unmatched([None, 3, None], 33747, "the aft-search baseline report")

    def test_report_is_allowed_when_the_index_is_empty(self) -> None:
        refuse_all_unmatched([None, None, None], 0, "the aft-search baseline report")

    def test_index_entry_count_reads_the_status_frame(self) -> None:
        self.assertEqual(index_entry_count({"status": "ready", "entries": 33747}), 33747)
        self.assertEqual(index_entry_count({"status": "disabled"}), 0)
        self.assertEqual(index_entry_count(None), 0)


class OrtRuntimeTests(unittest.TestCase):
    """The managed ONNX Runtime has a different file name on each platform."""

    def setUp(self) -> None:
        self.fusion = load_fusion_quality()

    def test_library_name_is_resolved_for_every_platform(self) -> None:
        self.assertEqual(self.fusion.ort_library_name("darwin"), "libonnxruntime.dylib")
        self.assertEqual(self.fusion.ort_library_name("linux"), "libonnxruntime.so")
        self.assertEqual(self.fusion.ort_library_name("win32"), "onnxruntime.dll")

    def test_windows_candidate_is_the_managed_dll_under_local_appdata(self) -> None:
        candidates = self.fusion.ort_library_candidates(
            "win32", {"LOCALAPPDATA": "C:/Users/bench/AppData/Local"}
        )
        self.assertEqual(
            [candidate.as_posix() for candidate in candidates],
            ["C:/Users/bench/AppData/Local/cortexkit/aft/onnxruntime/1.24.4/onnxruntime.dll"],
        )

    def test_unix_candidates_keep_the_managed_and_system_locations(self) -> None:
        candidates = [path.as_posix() for path in self.fusion.ort_library_candidates("darwin", {})]
        self.assertTrue(any(path.endswith("/cortexkit/aft/onnxruntime/1.24.4/libonnxruntime.dylib") for path in candidates))
        self.assertIn("/usr/local/lib/libonnxruntime.dylib", candidates)


class RealQueryPlatformTests(unittest.TestCase):
    """The real-query reference pair is Unix-captured; say so before building."""

    def test_windows_is_refused_with_a_reason(self) -> None:
        with self.assertRaisesRegex(UnsupportedPlatform, "Unix-captured"):
            assert_reference_platform("win32")

    def test_unix_platforms_are_accepted(self) -> None:
        assert_reference_platform("linux")
        assert_reference_platform("darwin")

    def test_windows_exits_with_a_distinct_code_before_any_index_build(self) -> None:
        self.assertNotIn(PLATFORM_UNSUPPORTED_EXIT, (0, 2))
        with mock.patch.object(sys, "platform", "win32"), mock.patch.object(sys, "argv", ["run_real_query.py"]):
            with mock.patch("run_real_query.ensure_binary") as ensure_binary:
                self.assertEqual(real_query_main(), PLATFORM_UNSUPPORTED_EXIT)
        ensure_binary.assert_not_called()


class AnswerKeyExclusionTests(unittest.TestCase):
    """The measured index must not contain the fixtures' own expected files."""

    def test_files_carrying_expected_top_files_are_ignored(self) -> None:
        entries = ignore_entries()
        carriers = [
            path.name
            for path in sorted(HERE.glob("*.json"))
            if '"expected_top_files"' in path.read_text(encoding="utf-8")
        ]
        self.assertTrue(carriers)
        for name in carriers:
            with self.subTest(file=name):
                self.assertIn(name, entries)

    def test_report_directory_and_recorded_baselines_are_ignored(self) -> None:
        entries = ignore_entries()
        self.assertIn("results/", entries)
        self.assertIn("external-fixtures.json", entries)
        self.assertIn("real-query-baseline.json", entries)

    def test_evidence_tree_copy_carries_the_ignore_list_without_changing_the_tree(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            tree = Path(directory) / "aft-evidence"
            fixtures = tree / "benchmarks" / "aft-search"
            fixtures.mkdir(parents=True)
            (fixtures / "fixtures.json").write_text("[]\n", encoding="utf-8")
            (tree / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
            pinned_digest = evidence_tree_sha256(tree)
            with runtime_evidence_tree(tree) as root:
                copied = root / "benchmarks" / "aft-search" / ".aftignore"
                self.assertTrue(copied.is_file())
                self.assertEqual(copied.read_bytes(), (HERE / ".aftignore").read_bytes())
            self.assertEqual(evidence_tree_sha256(tree), pinned_digest)


if __name__ == "__main__":
    unittest.main()
