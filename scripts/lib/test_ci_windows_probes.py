#!/usr/bin/env python3
"""Exercise archive-harness selection and serial probe dispatch without Cargo."""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("ci_windows_probes", ROOT / "scripts/ci-windows-probes.py")
assert SPEC is not None and SPEC.loader is not None
probes = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(probes)


class WindowsProbeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.folder = Path(self.temp.name)
        self.binary = self.folder / "aft-current.exe"
        self.binary.write_text("#!" + sys.executable + "\n" + '''
import json, os, pathlib, sys
with pathlib.Path(__file__).with_suffix(".calls").open("a") as handle:
    handle.write(json.dumps({"args": sys.argv[1:], "cwd": os.getcwd()}) + "\\n")
if "--list" in sys.argv:
    if not os.environ.get("FAKE_PROBE_MISSING"):
        print(sys.argv[-1] + ": test")
    sys.exit(int(os.environ.get("FAKE_PROBE_LIST_EXIT", "0")))
sys.exit(int(os.environ.get("FAKE_PROBE_EXIT", "0")))
''')
        self.binary.chmod(0o755)
        self.metadata = {"rust-binaries": {
            "agent-file-tools": {
                "binary-name": "aft", "kind": "lib", "binary-path": str(self.binary),
            },
            "agent-file-tools::bin/aft": {
                "binary-name": "aft", "kind": "bin", "binary-path": str(self.folder / "aft-bin.exe"),
            },
            "aft-tokenizer": {
                "binary-name": "aft_tokenizer", "kind": "lib", "binary-path": str(self.folder / "tokenizer.exe"),
            },
        }}

    def calls(self):
        path = self.binary.with_suffix(".calls")
        return [json.loads(row) for row in path.read_text().splitlines()] if path.exists() else []

    def archive(self, name="target/nextest/binaries-metadata.json", compressed=False):
        path = self.folder / "nextest.tar.zst"
        content = json.dumps(self.metadata).encode()
        with tarfile.open(path, "w") as archive:
            member = tarfile.TarInfo(name)
            member.size = len(content)
            archive.addfile(member, io.BytesIO(content))
        if compressed:
            zstd = importlib.import_module("compression.zstd")
            path.write_bytes(zstd.compress(path.read_bytes()))
        return path

    def test_reads_original_local_harness_path_from_archive_without_extracting_it(self):
        path = self.archive()
        self.assertEqual(probes.archive_library_harness(path), self.binary)
        self.assertFalse((self.folder / "target").exists())

    @unittest.skipUnless(sys.version_info >= (3, 14), "zstd support requires the producer's Python 3.14")
    def test_reads_zstd_archive_with_the_producer_python(self):
        self.assertEqual(probes.archive_library_harness(self.archive(compressed=True)), self.binary)

    def test_archive_without_binary_metadata_is_refused(self):
        path = self.archive(name="target/nextest/cargo-metadata.json")
        with self.assertRaisesRegex(ValueError, "no binaries metadata"):
            probes.archive_library_harness(path)

    def test_selects_only_metadata_library_not_stale_or_binary_harness(self):
        (self.folder / "aft-stale.exe").write_text("stale")
        self.assertEqual(probes.library_harness(self.metadata), self.binary)

    def test_missing_or_ambiguous_library_metadata_is_refused(self):
        del self.metadata["rust-binaries"]["agent-file-tools"]
        with self.assertRaisesRegex(ValueError, "found 0"):
            probes.library_harness(self.metadata)
        entry = {"binary-name": "aft", "kind": "lib", "binary-path": str(self.binary)}
        self.metadata["rust-binaries"].update({"first": entry, "second": entry})
        with self.assertRaisesRegex(ValueError, "found 2"):
            probes.library_harness(self.metadata)

    def test_missing_or_relative_executable_is_refused(self):
        entry = self.metadata["rust-binaries"]["agent-file-tools"]
        for path in ("aft-relative.exe", str(self.folder / "missing.exe")):
            with self.subTest(path=path):
                entry["binary-path"] = path
                with self.assertRaisesRegex(ValueError, "missing or not absolute"):
                    probes.library_harness(self.metadata)

    def test_each_probe_executes_exactly_once_without_cargo(self):
        with contextlib.redirect_stdout(io.StringIO()):
            for probe in ("tls", "backtrace"):
                self.assertEqual(probes.run_probe(self.binary, probe), 0)
        calls = self.calls()
        self.assertEqual([call["args"] for call in calls], [
            ["--list", "--exact", probes.PROBES["tls"]],
            ["--exact", probes.PROBES["tls"]],
            ["--list", "--exact", probes.PROBES["backtrace"]],
            ["--exact", probes.PROBES["backtrace"]],
        ])
        self.assertTrue(all(Path(call["cwd"]) == ROOT / "crates/aft" for call in calls), calls)

    def test_child_failure_is_not_treated_as_probe_success(self):
        with patch.dict(os.environ, {"FAKE_PROBE_EXIT": "17"}), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(probes.run_probe(self.binary, "backtrace"), 17)
        self.assertEqual(len(self.calls()), 2)

    def test_zero_matching_tests_is_refused_before_execution(self):
        with patch.dict(os.environ, {"FAKE_PROBE_MISSING": "1"}):
            with self.assertRaisesRegex(ValueError, "serial probe is missing"):
                probes.run_probe(self.binary, "backtrace")
        self.assertEqual(len(self.calls()), 1)

    def test_inventory_failure_is_not_treated_as_zero_tests(self):
        with patch.dict(os.environ, {"FAKE_PROBE_LIST_EXIT": "9"}):
            with self.assertRaises(subprocess.CalledProcessError):
                probes.run_probe(self.binary, "tls")
        self.assertEqual(len(self.calls()), 1)

    def test_cli_reads_the_saved_metadata_and_propagates_test_failure(self):
        archive = self.archive()
        result = subprocess.run(
            [sys.executable, str(ROOT / "scripts/ci-windows-probes.py"),
             "--archive-file", str(archive), "--probe", "backtrace"],
            env={**os.environ, "FAKE_PROBE_EXIT": "23"},
            capture_output=True, text=True, check=False,
        )
        self.assertEqual(result.returncode, 23, result.stdout + result.stderr)
        self.assertIn("Running serial Windows probe logging::tests::", result.stdout)
        self.assertIn("Windows serial probe runner: Python", result.stdout)
        self.assertEqual(len(self.calls()), 2)


if __name__ == "__main__":
    unittest.main()
