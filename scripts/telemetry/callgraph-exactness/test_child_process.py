"""Regression checks for descendants surviving an exactness command."""
from pathlib import Path
import subprocess
import sys
import time
import unittest

from child_process import child_process


@unittest.skipUnless(sys.platform == "linux", "the exactness runner is Linux")
class ChildProcessTests(unittest.TestCase):
    def assert_terminated(self, pid):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                state = Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1][0]
            except (FileNotFoundError, ProcessLookupError):
                return
            if state == "Z":
                return
            time.sleep(0.01)
        self.fail(f"descendant {pid} still running")

    def test_successful_parent_does_not_leave_a_descendant(self):
        command = [sys.executable, "-c", "import subprocess,sys; p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL); print(p.pid, flush=True)"]
        with child_process(command, stdout=subprocess.PIPE, text=True) as process:
            stdout = process.stdout
            assert stdout is not None
            child = int(stdout.readline())
            self.assertEqual(process.wait(timeout=5), 0)
        stdout.close()
        self.assert_terminated(child)

    def test_exception_terminates_parent_and_descendant(self):
        command = [sys.executable, "-c", "import subprocess,sys,time; p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL); print(p.pid, flush=True); time.sleep(60)"]
        process = None
        stdout = None
        child = None
        with self.assertRaisesRegex(RuntimeError, "probe failed"):
            with child_process(command, stdout=subprocess.PIPE, text=True) as process:
                stdout = process.stdout
                assert stdout is not None
                child = int(stdout.readline())
                raise RuntimeError("probe failed")
        assert process is not None and stdout is not None and child is not None
        stdout.close()
        self.assertLess(process.returncode, 0)
        self.assert_terminated(child)


if __name__ == "__main__":
    unittest.main()
