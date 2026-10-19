#!/usr/bin/env python3
"""Exercise CI gate collection with failing fake runners, not real heavy suites."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class RustGateCollectionTests(unittest.TestCase):
    def run_gate(self, phases, ci=True, fail=False, archives=False, tee_fail=False, cargo_only=False):
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
            if tee_fail:
                stubs["tee"] = "#!/bin/sh\ncat\nexit 23\n"
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
                "AFT_GATE_HOME_ROOT": str(home / "homes"), "AFT_GATE_LOG": str(home / "gate.log"),
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
            result = subprocess.run(["bash", str(ROOT / "scripts/rust-test-gate.sh")],
                                    env=env, cwd=ROOT, text=True, capture_output=True, timeout=30)
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

    def test_ci_log_write_failure_stops_instead_of_hiding_missing_evidence(self):
        result, calls = self.run_gate("conformance", tee_fail=True)
        self.assertEqual(result.returncode, 23, result.stdout + result.stderr)
        self.assertEqual(len(calls), 1)

    def test_cargo_only_ci_exit_does_not_erase_a_failed_suite(self):
        result, calls = self.run_gate("conformance", cargo_only=True, fail=True)
        self.assertEqual(result.returncode, 7, result.stdout + result.stderr)
        self.assertEqual(len(calls), 1)
        self.assertIn("--no-fail-fast", calls[0])
        self.assertIn("GATE FAILED", result.stdout)


if __name__ == "__main__":
    unittest.main()
