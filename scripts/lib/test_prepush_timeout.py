"""The macOS E2E fallback must enforce deadlines, including grandchildren."""
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


TIMEOUT = Path(__file__).resolve().parents[2] / "tests/macos-e2e/bin/timeout"


class PrepushTimeoutTests(unittest.TestCase):
    def run_timeout(self, *args):
        return subprocess.run([sys.executable, str(TIMEOUT), *args], text=True, capture_output=True, timeout=10)

    def test_normal_exit_and_output_are_preserved(self):
        result = self.run_timeout("--signal=KILL", "5", sys.executable, "-c", "print('completed'); raise SystemExit(7)")
        self.assertEqual(result.returncode, 7)
        self.assertEqual(result.stdout, "completed\n")

    def test_deadline_kills_the_child_and_its_grandchild(self):
        scratch = Path("target/scratch")
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temp:
            marker = Path(temp).resolve() / "grandchild-survived"
            spawned = Path(temp).resolve() / "grandchild-spawned"
            grandchild = f"import time; from pathlib import Path; time.sleep(2); Path({str(marker)!r}).write_text('survived')"
            parent = f"import subprocess,sys,time; from pathlib import Path; subprocess.Popen([sys.executable,'-c',{grandchild!r}]); Path({str(spawned)!r}).write_text('spawned'); time.sleep(30)"
            result = self.run_timeout("--signal=KILL", "1", sys.executable, "-c", parent)
            self.assertEqual(result.returncode, 137)
            self.assertTrue(spawned.exists(), "fixture never created the grandchild")
            # A surviving grandchild retains the captured output pipe, so run()
            # could not return before that process wrote the marker and exited.
            self.assertFalse(marker.exists())

    def test_missing_command_returns_127(self):
        self.assertEqual(self.run_timeout("--signal=KILL", "5", "/aft-prepush-no-such-command").returncode, 127)

    def test_unsupported_options_fail_instead_of_running_unbounded(self):
        self.assertEqual(self.run_timeout("--signal=TERM", "1", sys.executable, "-c", "pass").returncode, 125)


if __name__ == "__main__":
    unittest.main()
