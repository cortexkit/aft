"""Offline checks for the native cold-index benchmark harness."""
from __future__ import annotations

import csv
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import native
import run as bench
import sampling


class NativeTests(unittest.TestCase):
    def test_windows_guard_records_either_and_both_peak_counters(self):
        limit = 26 * native.GIB
        self.assertEqual(native.guard_metrics("windows", limit - 1, limit, limit), ["peak_commit_bytes"])
        self.assertEqual(native.guard_metrics("windows", limit, limit - 1, limit), ["peak_working_set_bytes"])
        self.assertEqual(native.guard_metrics("windows", limit, limit, limit),
                         ["peak_working_set_bytes", "peak_commit_bytes"])
        self.assertEqual(native.guard_metrics("macos", limit - 1, None, limit), [])
        self.assertEqual(native.guard_metrics("macos", limit, None, limit), ["rss_bytes"])

    def test_waiter_keeps_child_peak_and_cpu_after_exit(self):
        scratch = HERE.parents[2] / "target/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            with (Path(directory) / "stderr.log").open("w") as log:
                child = native.Child([sys.executable, "-c", "x = bytearray(8 * 1024 * 1024)"], directory, None, log, 1)
                self.assertTrue(child.done.wait(10))
                result = child.finish()
            self.assertEqual(result["exit_code"], 0)
            self.assertGreater(result["peak_memory_bytes"], 8 * 1024 * 1024)
            self.assertGreaterEqual(result["cpu_seconds"], 0)
            child.stdout.close()
            self.assertFalse(result["guard_triggered"])
            self.assertEqual(result["counter_errors"], [])

    def test_guard_kills_child_and_preserves_peak_at_kill(self):
        scratch = HERE.parents[2] / "target/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            with (Path(directory) / "stderr.log").open("w") as log:
                child = native.Child([sys.executable, "-c", "import time; x = bytearray(8 * 1024 * 1024); time.sleep(20)"],
                                     directory, None, log, 0.001)
                self.assertTrue(child.done.wait(10))
                result = child.finish()
                child.stdout.close()
            self.assertNotEqual(result["exit_code"], 0)
            self.assertTrue(result["guard_triggered"])
            self.assertGreater(result["peak_at_guard"]["peak_memory_bytes"], 1024 * 1024)

    def test_timeline_records_cpu_and_rusage_counters(self):
        scratch = HERE.parents[2] / "target/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            timeline = Path(directory) / "timeline.csv"
            with (Path(directory) / "stderr.log").open("w") as log:
                child = native.Child([sys.executable, "-c", "import time\nend = time.time() + 2.5\nwhile time.time() < end: pass"],
                                     directory, None, log, 1, timeline)
                self.assertTrue(child.done.wait(10))
                result = child.finish()
                child.stdout.close()
            with timeline.open() as stream:
                rows = list(csv.DictReader(stream))
            self.assertGreaterEqual(len(rows), 2)
            self.assertGreater(float(rows[-1]["cpu_seconds"]), 0.5)
            self.assertGreater(result["rusage"]["user_seconds"], 0.5)

    def test_ps_time_parsing(self):
        self.assertEqual(native.ps_cpu_seconds("0:01.50"), 1.5)
        self.assertEqual(native.ps_cpu_seconds(" 12:03.25 "), 723.25)
        self.assertEqual(native.ps_cpu_seconds("1:00:00"), 3600)
        self.assertEqual(native.ps_cpu_seconds("2-00:00:01"), 2 * 86400 + 1)


class SamplingTests(unittest.TestCase):
    def test_stage_changes_report_only_moments_a_field_changes(self):
        with tempfile.TemporaryDirectory(dir=HERE.parents[2] / "target") as directory:
            path = Path(directory) / "status.jsonl"
            records = [(1_000_000_000, {"search_index": {"status": "building", "files": 1}, "other": {"status": "x"}}),
                       (1_500_000_000, {"search_index": {"status": "building", "files": 9}}),
                       (3_000_000_000, {"search_index": {"status": "ready", "files": 9},
                                        "callgraph_store": {"status": "building", "stage": {"name": "pass4"}}})]
            path.write_text("".join(json.dumps({"monotonic_ns": ns, "response": response}) + "\n"
                                    for ns, response in records))
            self.assertEqual(sampling.stage_changes(path, 500_000_000), [
                {"seconds": 0.5, "field": "search_index.status", "value": "building"},
                {"seconds": 2.5, "field": "callgraph_store.stage.name", "value": "pass4"},
                {"seconds": 2.5, "field": "callgraph_store.status", "value": "building"},
                {"seconds": 2.5, "field": "search_index.status", "value": "ready"}])

    def test_symbolize_replaces_only_unresolved_frames_of_known_images(self):
        text = ("    5 ???  (in aft-candidate)  load address 0x100000000 + 0x10  [0x100000010]\n"
                "    3 ???  (in libsystem_kernel.dylib)  load address 0x180000000 + 0x20  [0x180000020]\n"
                "    2 main  (in aft-candidate) + 4  [0x100000004]\n")
        calls = []
        def fake_atos(command, **_):
            calls.append(command)
            return subprocess.CompletedProcess(command, 0, stdout="aft::build::run (in aft) (build.rs:7)\n")
        resolved, count = sampling.symbolize(text, {"aft-candidate": Path("aft.dSYM/DWARF/aft")}, run=fake_atos)
        self.assertEqual(count, 1)
        self.assertEqual(calls, [["atos", "-o", "aft.dSYM/DWARF/aft", "-l", "0x100000000", "0x100000010"]])
        self.assertIn("5 aft::build::run  (in aft-candidate)  [0x100000010]", resolved)
        self.assertIn("???  (in libsystem_kernel.dylib)", resolved)


class HarnessTests(unittest.TestCase):
    def setUp(self):
        scratch = HERE.parents[2] / "target/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def test_default_project_and_skip_policy(self):
        for system, expected in (("macos", ["vscode", "kubernetes", "elasticsearch", "linux"]),
                                 ("windows", ["vscode", "kubernetes", "elasticsearch", "linux", "chromium"])):
            plan = bench.plan(system)
            self.assertEqual([project for project, _ in plan], expected)
            self.assertEqual(plan[0][1], ["0.65.0", "0.66-candidate"])
            self.assertEqual(plan[1][1], ["0.66-candidate", "0.65.0"])
            self.assertNotIn("elasticsearch", bench.BASELINE[system])
        self.assertEqual(bench.BASELINE["macos"], {"kubernetes", "linux"})
        self.assertEqual(bench.BASELINE["windows"], {"vscode", "kubernetes", "linux", "chromium"})
        self.assertEqual(bench.plan("windows", ["chromium", "linux"])[-1][0], "chromium")
        self.assertEqual(bench.plan("linux"), [("neovim", ["0.65.0", "0.66-candidate"])])
        with self.assertRaises(ValueError):
            bench.plan("linux", ["linux"])
        self.assertEqual(bench.plan("linux", ["linux"], profile=True), [("linux", ["0.65.0", "0.66-candidate"])])
        with self.assertRaises(ValueError):
            bench.plan("macos", ["chromium"])

    def test_dwarf_lookup_needs_a_dsym_next_to_the_binary(self):
        binary = self.root / "aft-candidate"
        binary.write_bytes(b"")
        self.assertIsNone(bench.dwarf_for(binary))
        dwarf = self.root / "aft-candidate.dSYM/Contents/Resources/DWARF"
        dwarf.mkdir(parents=True)
        (dwarf / "aft").write_bytes(b"")
        self.assertEqual(bench.dwarf_for(binary), {"aft-candidate": dwarf / "aft"})

    def test_release_asset_names_and_exact_checksum_selection(self):
        self.assertEqual(bench.release_asset("windows", "AMD64"), "aft-win32-x64.exe")
        self.assertEqual(bench.release_asset("macos", "arm64"), "aft-darwin-arm64")
        self.assertEqual(bench.release_asset("linux", "x86_64"), "aft-linux-x64")
        checksum = "a" * 64
        text = checksum + "  aft-linux-x64\n" + "b" * 64 + "  aft-linux-x64-debug.zip\n"
        self.assertEqual(bench.checksum_for(text, "aft-linux-x64"), checksum)
        with self.assertRaises(ValueError):
            bench.checksum_for(text, "aft-win32-x64.exe")
        with self.assertRaises(ValueError):
            bench.checksum_for(text + text, "aft-linux-x64")

    def test_windows_path_filter_includes_devices_inside_directories(self):
        for path in ("drivers/aux.c", "a/CON", "LPT1/file.c", "a/nul.txt", "a/com9.h", "a/b.", "a/b ", "a/b:c"):
            self.assertTrue(bench.windows_invalid(path), path)
        for path in ("auxiliary.c", "a/normal.c", "a/company.h", "a/com10.h"):
            self.assertFalse(bench.windows_invalid(path), path)
        self.assertEqual(bench.sparse_literal("a/b[1].c"), "!/a/b\\[1\\].c")
        self.assertEqual(bench.windows_exclusions(["safe.c", "aux.c", "a/X.h", "a/x.h"]), ["aux.c", "a/X.h", "a/x.h"])

    def test_sparse_checkout_fetches_pin_and_omits_invalid_tracked_paths(self):
        source, logs = self.root / "source", self.root / "logs"
        source.mkdir()
        logs.mkdir()
        def git(*args, data=None):
            return subprocess.check_output(["git", "-C", str(source), *args], input=data)
        git("init", "-q")
        blob = git("hash-object", "-w", "--stdin", data=b"int main() {}\n").decode().strip()
        for path in ("safe.c", "aux.c", "a[1].c"):
            git("update-index", "--add", "--cacheinfo", f"100644,{blob},{path}")
        tree = git("write-tree").decode().strip()
        commit = subprocess.check_output(["git", "-c", "user.name=bench", "-c", "user.email=bench@example.invalid",
                                          "-C", str(source), "commit-tree", tree, "-m", "fixture"]).decode().strip()
        entry = {"repository": "fixture/local", "clone_url": source.as_uri(), "commit": commit}
        destination = self.root / "checkout"
        self.assertEqual(bench.checkout(entry, destination, logs, windows=True), 1)
        self.assertTrue((destination / "safe.c").is_file())
        self.assertTrue((destination / "a[1].c").is_file())
        self.assertFalse((destination / "aux.c").exists())
        self.assertEqual(json.loads((logs / "checkout.json").read_text())["excluded_paths"], ["aux.c"])
        self.assertEqual(subprocess.check_output(["git", "-C", str(destination), "rev-parse", "--is-shallow-repository"],
                                                text=True).strip(), "true")

    def test_table_preserves_zero_and_missing_and_skip_reason(self):
        rows = [{"project": "linux", "version": "0.65.0", "outcome": "skipped: exceeds machine"},
                {"project": "linux", "version": "0.66-candidate", "outcome": "ready", "peak_memory_bytes": native.GIB,
                 "wall_seconds": 0, "cpu_seconds": 1.5}]
        bench.write_tables(self.root, rows)
        with (self.root / "results.csv").open() as stream:
            saved = list(csv.DictReader(stream))
        self.assertEqual(saved[0]["wall_seconds"], "")
        self.assertEqual(saved[1]["wall_seconds"], "0")
        self.assertIn("1.000 GiB", (self.root / "results.md").read_text())
        self.assertIn("skipped: exceeds machine", (self.root / "results.md").read_text())


if __name__ == "__main__":
    unittest.main()
