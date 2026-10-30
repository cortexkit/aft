#!/usr/bin/env python3
"""Exercise CI gate collection with failing fake runners, not real heavy suites."""

import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[2]


class RustGateCollectionTests(unittest.TestCase):
    def run_gate(self, phases, ci=True, fail=False, archives=False, cargo_only=False, leak=False,
                 log_dir_mode=None, macos=False):
        with tempfile.TemporaryDirectory() as temp:
            home = Path(temp)
            tools = home / "bin"
            tools.mkdir()
            calls = home / "calls.jsonl"
            stubs = {
                "git": '#!/bin/sh\nprintf "%s\\n" "$FAKE_REPO_ROOT"\n',
                "cargo-nextest": "#!/bin/sh\nexit 0\n",
                "cargo": '''#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
with open(os.environ["FAKE_CALL_LOG"], "a") as out:
    out.write(json.dumps(args) + "\\n")
if args[:2] == ["nextest", "list"] and "--extract-to" in args:
    from pathlib import Path
    metadata = Path(args[args.index("--extract-to") + 1]) / "target/nextest"
    metadata.mkdir(parents=True)
    (metadata / "cargo-metadata.json").write_text("{}")
    (metadata / "binaries-metadata.json").write_text(json.dumps({
        "rust-build-meta": {"target-directory": "/builder/target"}, "rust-binaries": {},
    }))
leak_pid = os.environ.get("FAKE_LEAK_PID")
if leak_pid and not os.path.exists(leak_pid):
    import subprocess
    # Like a leaked test grandchild: a long-lived process that keeps this
    # runner's stdout and stderr open after the runner itself exits.
    child = subprocess.Popen(["sleep", "30"])
    with open(leak_pid, "w") as out:
        out.write(str(child.pid))
    print("phase output before the leak")
    sys.stdout.flush()
    sys.exit(0)
if os.environ.get("FAKE_FAIL") == "1":
    if args[0] == "test" and "--workspace" in args:
        sys.exit(7)
    if "scope_end_kill" in args:
        print("synthetic first phase failure")
        sys.exit(7)
    if "scope_revocation" in args:
        print("synthetic second phase failure")
        sys.exit(9)
sys.exit(0)
''',
            }
            if macos:
                stubs["uname"] = '#!/bin/sh\nprintf "Darwin\\n"\n'
            for name, body in stubs.items():
                path = tools / name
                path.write_text(body)
                path.chmod(0o755)
            env = dict(os.environ)
            env.update({
                "PATH": str(tools) + os.pathsep + env["PATH"],
                "FAKE_REPO_ROOT": str(ROOT), "FAKE_CALL_LOG": str(calls),
                "FAKE_FAIL": "1" if fail else "0", "AFT_GATE_DEMOTED": "1",
                "AFT_SKIP_BOX_GATE": "1", "AFT_GATE_PHASES": phases,
                "AFT_GATE_HOME_ROOT": str(home / "homes"), "AFT_GATE_LOG": str(home / "logs" / "gate.log"),
                "AFT_RUST_TEST_RUNNER": "cargo" if cargo_only else "nextest", "AFT_UNIT_TEST_RUNNER": "nextest",
            })
            for name in ("CI", "AFT_NEXTEST_ARCHIVE_FILE", "AFT_NEXTEST_ARCHIVE_BIN_FILE",
                         "AFT_NEXTEST_EXTRACT_TO", "AFT_NEXTEST_PARTITION", "AFT_GATE_SKIP_RELEASE_STORM"):
                env.pop(name, None)
            if ci:
                env["CI"] = "true"
            if archives:
                env.update({"AFT_NEXTEST_ARCHIVE_FILE": str(home / "lib.tar.zst"),
                            "AFT_NEXTEST_ARCHIVE_BIN_FILE": str(home / "bin.tar.zst"),
                            "AFT_NEXTEST_PARTITION": "hash:1/3"})
            if macos:
                env["AFT_NEXTEST_EXTRACT_TO"] = str(home / "extracted")
            if log_dir_mode is not None:
                (home / "logs").mkdir(mode=log_dir_mode)
            leak_pid = home / "leak.pid"
            if leak:
                env["FAKE_LEAK_PID"] = str(leak_pid)
            started = time.monotonic()
            try:
                result = subprocess.run(["bash", str(ROOT / "scripts/rust-test-gate.sh")],
                                        env=env, cwd=ROOT, text=True, capture_output=True, timeout=60)
            finally:
                if leak_pid.exists():
                    try:
                        os.kill(int(leak_pid.read_text()), signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                if log_dir_mode is not None:
                    (home / "logs").chmod(0o700)
            result.elapsed = time.monotonic() - started
            log = home / "logs" / "gate.log"
            result.gate_log = log.read_text() if log.exists() else ""
            observed = [json.loads(row) for row in calls.read_text().splitlines()] if calls.exists() else []
            return result, observed

    def test_ci_runs_every_conformance_phase_and_preserves_first_failure(self):
        result, calls = self.run_gate("conformance", fail=True)
        self.assertEqual(len(calls), 4, result.stdout + result.stderr)
        self.assertEqual(result.returncode, 7, result.stdout + result.stderr)
        self.assertTrue(all("--no-fail-fast" in call for call in calls), calls)
        self.assertIn("scope_revocation", calls[1])
        self.assertIn("tool_provider_conformance", calls[2])
        self.assertIn("GATE FAILED", result.stdout)

    def test_every_ci_nextest_archive_and_storm_run_collects_all_failures(self):
        result, calls = self.run_gate("lib-shard,nextest,storm", archives=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(len(calls), 4, calls)
        self.assertTrue(all(call[:2] == ["nextest", "run"] for call in calls), calls)
        self.assertTrue(all("--no-fail-fast" in call for call in calls), calls)
        self.assertIn("GATE PASSED", result.stdout)

    def test_local_gate_keeps_fail_fast_phase_behavior(self):
        result, calls = self.run_gate("conformance", ci=False, fail=True)
        self.assertEqual(result.returncode, 7)
        self.assertEqual(len(calls), 1)
        self.assertNotIn("--no-fail-fast", calls[0])

    def test_ci_unwritable_gate_log_stops_before_running_a_phase(self):
        result, calls = self.run_gate("conformance", log_dir_mode=0o500)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, [])

    def test_error_before_any_phase_is_kept_in_the_gate_log(self):
        # CI uploads only the gate log, so the reason a gate stopped before its
        # first phase must be in it, not only on stderr.
        result, calls = self.run_gate("bogus")
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertEqual(calls, [])
        self.assertIn("Unsupported AFT_GATE_PHASES entry 'bogus'", result.stderr)
        self.assertIn("Unsupported AFT_GATE_PHASES entry 'bogus'", result.gate_log)

    def test_leaked_child_holding_runner_output_does_not_hold_the_gate_open(self):
        # The leaked child lives 30s. A gate that pipes phase output through a
        # reader waiting for end-of-file (`tee`) stays open until it exits.
        result, calls = self.run_gate("watcher", leak=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(len(calls), 1, calls)
        self.assertLess(result.elapsed, 15, result.stdout + result.stderr)
        self.assertIn("phase output before the leak", result.stdout)
        self.assertIn("phase output before the leak", result.gate_log)
        self.assertIn("GATE PASSED", result.stdout)
        self.assertIn("GATE PASSED", result.gate_log)

    def test_libtest_archive_partition_never_compiles_and_excludes_serial_probes(self):
        result, calls = self.run_gate("lib-shard", archives=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(len(calls), 2, calls)
        self.assertTrue(all(call[:2] == ["nextest", "run"] for call in calls), calls)
        self.assertTrue(all("--archive-file" in call for call in calls), calls)
        library = next(call for call in calls if "kind(lib)" in call)
        self.assertEqual(library[library.index("--") + 1:], [
            "--skip", "platform_verifier_tls_client_subprocess",
            "--skip", "panic_hook_preserves_default_backtrace_behavior_and_persists",
        ])
        self.assertIn("GATE PASSED", result.stdout)

    def test_cargo_only_ci_exit_does_not_erase_a_failed_suite(self):
        result, calls = self.run_gate("conformance", cargo_only=True, fail=True)
        self.assertEqual(result.returncode, 7, result.stdout + result.stderr)
        self.assertEqual(len(calls), 1)
        self.assertIn("--no-fail-fast", calls[0])
        self.assertIn("GATE FAILED", result.stdout)

    def test_macos_libtest_extracts_once_per_archive_and_runs_prewarmed_metadata(self):
        result, calls = self.run_gate("lib,lib-shard", archives=True, macos=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        extracts = [call for call in calls if call[:2] == ["nextest", "list"]]
        runs = [call for call in calls if call[:2] == ["nextest", "run"]]
        self.assertEqual(len(extracts), 2, calls)
        roots = [call[call.index("--extract-to") + 1] for call in extracts]
        self.assertEqual(len(set(roots)), 2, "lib and bin archives must not replace each other")
        self.assertEqual(len(runs), 3, calls)
        for call, root in zip(runs, [roots[0], roots[0], roots[1]]):
            self.assertNotIn("--archive-file", call, "running an archive extracts fresh, unwarmed inodes")
            for flag, relative in (("--cargo-metadata", "target/nextest/cargo-metadata.json"),
                                   ("--binaries-metadata", "target/nextest/binaries-metadata.json"),
                                   ("--target-dir-remap", "target")):
                self.assertEqual(call[call.index(flag) + 1], str(Path(root) / relative), calls)
        self.assertEqual(result.stdout.count("==> warm macOS extracted libtest binaries"), 2)
        self.assertEqual(result.stdout.count("Filesystem"), 2, "log workspace disk space before each extraction")
        self.assertIn("GATE PASSED", result.stdout)


if __name__ == "__main__":
    unittest.main()
