"""Regression checks for gate verdicts and non-destructive byte comparisons."""
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("check_generated", HERE / "check-generated.py")
assert spec is not None and spec.loader is not None
generated = importlib.util.module_from_spec(spec)
spec.loader.exec_module(generated)


class PrepushTests(unittest.TestCase):
    def test_byte_drift_fails_and_restores_existing_and_new_files(self):
        scratch = Path("target/scratch")
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temp:
            root = Path(temp) / "fixtures"
            root.mkdir()
            original = root / "expected.json"
            original.write_bytes(b'{"exact":true}\r\n')
            command = [sys.executable, "-c", (
                "from pathlib import Path; "
                f"p=Path({str(root)!r}); "
                "(p/'expected.json').write_bytes(b'{\"exact\":true}\\n'); "
                "(p/'new.json').write_text('new')"
            )]
            self.assertNotEqual(generated.check(root, command), 0)
            self.assertEqual(original.read_bytes(), b'{"exact":true}\r\n')
            self.assertFalse((root / "new.json").exists())

    def test_failed_generator_is_not_a_clean_byte_check(self):
        scratch = Path("target/scratch")
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temp:
            root = Path(temp)
            (root / "expected").write_bytes(b"unchanged")
            self.assertEqual(generated.check(root, [sys.executable, "-c", "raise SystemExit(7)"]), 7)
            self.assertEqual((root / "expected").read_bytes(), b"unchanged")

    def test_clean_generator_passes(self):
        scratch = Path("target/scratch")
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temp:
            (Path(temp) / "expected").write_bytes(b"unchanged")
            self.assertEqual(generated.check(Path(temp), [sys.executable, "-c", "pass"]), 0)

    def test_multiple_file_generator_restores_each_original(self):
        scratch = Path("target/scratch")
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temp:
            paths = [Path(temp) / name for name in ("catalog.json", "presets.json")]
            for path in paths:
                path.write_bytes(b"original\n")
            command = [sys.executable, "-c", f"from pathlib import Path; Path({str(paths[0])!r}).write_bytes(b'drift\\n')"]
            self.assertNotEqual(generated.check(paths, command), 0)
            for path in paths:
                self.assertEqual(path.read_bytes(), b"original\n")

    def test_failed_step_stays_red_when_collection_continues(self):
        scratch = Path("target/scratch")
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temp:
            result = subprocess.run([
                "bash", "-c", 'source "$1/common.sh"; PREPUSH_LOG_DIR="$2"; '
                'prepush_init selftest; prepush_step deliberate-failure bash -c "exit 7" || true; prepush_finish',
                "test", str(HERE), temp,
            ], text=True, capture_output=True)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(result.stdout.splitlines()[-1], "GATE FAILED: selftest deliberate-failure")

    def test_zero_matching_native_drift_tests_is_not_a_pass(self):
        scratch = Path("target/scratch")
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temp:
            temp = Path(temp).resolve()
            cargo = temp / "cargo"
            cargo.write_text("#!/bin/sh\necho 'test result: ok. 0 passed; 0 failed'\n")
            cargo.chmod(0o755)
            env = {**os.environ, "PATH": f"{temp}{os.pathsep}{os.environ['PATH']}", "PREPUSH_LOG_DIR": str(temp / "logs")}
            result = subprocess.run(["bash", str(HERE / "rust-drift.sh"), "missing_sample_check"], env=env, text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("ran zero tests", result.stderr)

    def test_new_ci_feature_fails_until_added_to_matrix(self):
        scratch = Path("target/scratch")
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temp:
            temp = Path(temp).resolve()
            workflows = temp / ".github/workflows"
            workflows.mkdir(parents=True)
            (workflows / "new.yml").write_text("run: cargo check --features new-feature\n")
            result = subprocess.run([sys.executable, str(HERE / "check-feature-matrix.py")], cwd=temp, text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Uncovered/dynamic CI feature set 'new-feature'", result.stderr)

    def test_print_lanes_never_invokes_a_submit_command(self):
        scratch = Path("target/scratch")
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temp:
            temp = Path(temp).resolve()
            marker = temp / "submitted"
            ck = temp / "ck"
            ck.write_text(f"#!/bin/sh\ntouch '{marker}'\nexit 91\n")
            ck.chmod(0o755)
            env = {**os.environ, "PATH": f"{temp}{os.pathsep}{os.environ['PATH']}",
                   "PREPUSH_BASE": "HEAD", "PREPUSH_BRANCH": "prepush-plan-fixture"}
            result = subprocess.run(["bash", str(HERE.parent / "prepush.sh"), "--print-lanes"], env=env, text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            rows = [json.loads(line) for line in result.stdout.splitlines()]
            self.assertEqual(len(rows), 6)
            self.assertEqual([row["number"] for row in rows], [1, 2, 3, 4, 5, 6])
            self.assertEqual([row["lane"] for row in rows], ["fast-compile", "linux-rust", "linux-typescript", "linux-search", "windows", "macos"])
            self.assertEqual(rows[0]["vcpus"], 4)
            self.assertEqual({r["runon"] for r in rows}, {"linux,4c,net", "linux,16c,net", "windows", "macos"})
            for row in rows:
                self.assertIn("PREPUSH_COMMIT=", row["command"])
                self.assertIn("PREPUSH_BASE=", row["command"])
                self.assertIn("PREPUSH_BRANCH=", row["command"])
                self.assertNotIn("ck motor", row["command"])
            self.assertFalse(marker.exists())

    def test_script_submit_mode_does_not_exist(self):
        result = subprocess.run(["bash", str(HERE.parent / "prepush.sh"), "--submit"], text=True, capture_output=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("never submits jobs", result.stdout)
        self.assertFalse((HERE / "submit.sh").exists())

    def test_stale_printed_commit_is_refused_before_a_lane_runs(self):
        scratch = Path("target/scratch")
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temp:
            env = {**os.environ, "PREPUSH_COMMIT": "0" * 40, "PREPUSH_LOG_DIR": str(Path(temp).resolve())}
            result = subprocess.run(["bash", "-c", 'source "$1/common.sh"; prepush_init stale', "test", str(HERE)], env=env, text=True, capture_output=True)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(result.stdout.splitlines()[-1], "GATE FAILED: stale provenance")

    def test_direct_lib_phase_builds_real_aft_before_library_tests(self):
        gate = (HERE.parent / "rust-test-gate.sh").read_text()
        lib = gate[gate.index("if phase_enabled lib; then"):]
        build = "gate_cargo build -p agent-file-tools --bin aft"
        self.assertIn(build, lib)
        self.assertIn('if [[ -z "${AFT_NEXTEST_ARCHIVE_FILE:-}" ]]; then', lib[:lib.index(build)])
        linux = (HERE / "linux-rust.sh").read_text()
        self.assertLess(linux.index("prepush_step debug-binary cargo build -p agent-file-tools --bin aft"), linux.index("prepush_step archive"))
        scratch = Path("target/scratch")
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temp:
            temp = Path(temp).resolve()
            calls = temp / "calls"
            cargo = temp / "cargo"
            cargo.write_text(f"#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{calls}'\necho 'test result: ok. 1 passed; 0 failed'\n")
            cargo.chmod(0o755)
            nextest = temp / "cargo-nextest"
            nextest.write_text("#!/bin/sh\nexit 0\n")
            nextest.chmod(0o755)
            env = {**os.environ, "PATH": f"{temp}{os.pathsep}{os.environ['PATH']}",
                   "CI": "1", "AFT_SKIP_BOX_GATE": "1", "AFT_GATE_DEMOTED": "1",
                   "AFT_GATE_NO_XPROTECT_REMEDIATION": "1", "AFT_RUST_TEST_RUNNER": "nextest",
                   "AFT_UNIT_TEST_RUNNER": "cargo", "AFT_GATE_PHASES": "lib",
                   "AFT_GATE_LOG": str(temp / "gate.log"), "AFT_GATE_HOME_ROOT": str(temp / "homes")}
            env.pop("AFT_NEXTEST_ARCHIVE_FILE", None)
            result = subprocess.run(["bash", str(HERE.parent / "rust-test-gate.sh")], env=env, text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            commands = calls.read_text().splitlines()
            self.assertEqual(commands[0], "build -p agent-file-tools --bin aft")
            self.assertTrue(any("--lib --bins" in command for command in commands))

    def test_remote_rust_gate_never_invokes_biome(self):
        for path in (HERE.parent / "rust-test-gate.sh", HERE / "linux-rust.sh", HERE / "windows.sh", HERE / "macos.sh"):
            self.assertNotIn("bunx biome", path.read_text(), str(path))
            self.assertNotIn("bun run lint", path.read_text(), str(path))


if __name__ == "__main__":
    unittest.main()
