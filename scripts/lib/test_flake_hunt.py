"""Keep moved library tests reachable through the filtered flake-hunt helper."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class FlakeHuntPackages(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.folder = Path(self.temp.name)
        cargo = self.folder / "cargo"
        cargo.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
if sys.argv[1:3] == ["nextest", "run"]:
    pathlib.Path(os.environ["FAKE_NEXTEST_ARGS"]).write_text(json.dumps(sys.argv[1:]))
    print("fake nextest run")
    sys.exit(int(os.environ.get("FAKE_NEXTEST_EXIT", "0")))
print("cargo-nextest fake")
''')
        cargo.chmod(0o755)
        self.arguments = self.folder / "arguments.json"

    def run_hunt(self, package=None, status=0):
        env = dict(os.environ)
        env.pop("AFT_FLAKE_HUNT_PACKAGE", None)
        env.update(PATH=str(self.folder) + os.pathsep + env["PATH"],
                   AFT_FLAKE_HUNT_LOG_DIR=str(self.folder / "logs"),
                   FAKE_NEXTEST_ARGS=str(self.arguments), FAKE_NEXTEST_EXIT=str(status))
        if package is not None:
            env["AFT_FLAKE_HUNT_PACKAGE"] = package
        result = subprocess.run(
            ["bash", str(ROOT / "scripts/flake-hunt.sh"), "bounded_io::", "1"],
            cwd=ROOT, env=env, capture_output=True, text=True, timeout=20,
        )
        return result, json.loads(self.arguments.read_text())

    def test_default_filter_selects_all_test_owning_packages(self):
        result, arguments = self.run_hunt()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        selected = [arguments[index + 1] for index, value in enumerate(arguments) if value == "-p"]
        self.assertEqual(selected, ["agent-file-tools", "aft-base", "aft-db", "aft-engine", "aft-tools", "aft-subc"])
        self.assertIn("bounded_io::", arguments)
        self.assertIn("package: agent-file-tools + aft-base + aft-db + aft-engine + aft-tools + aft-subc", result.stdout)

    def test_explicit_package_still_selects_only_that_package(self):
        result, arguments = self.run_hunt(package="aft-base")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        selected = [arguments[index + 1] for index, value in enumerate(arguments) if value == "-p"]
        self.assertEqual(selected, ["aft-base"])

    def test_nextest_failure_is_not_hidden_by_log_capture(self):
        result, _ = self.run_hunt(status=17)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("failures: 1", result.stdout)


if __name__ == "__main__":
    unittest.main()
