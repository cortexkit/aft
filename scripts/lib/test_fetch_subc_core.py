#!/usr/bin/env python3
"""Exercise staged daemon archives without GitHub access or real release assets."""

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[2]
TAG = "subc-core-v0.21.3"
ARCHIVE = "ck-subc-linux-x64.zip"
PIN = "2d5e01bf027e7ad2f74ce2907c33b484c3017c64520ac4da84795e69c39aad79"


class FetchSubcCoreTests(unittest.TestCase):
    def setUp(self):
        scratch = ROOT / "target" / "scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        self.tmp = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        self.fixture = self.root / "fixture.zip"
        with zipfile.ZipFile(self.fixture, "w") as archive:
            archive.writestr("ck-subc", "fixture daemon\n")
        self.digest = hashlib.sha256(self.fixture.read_bytes()).hexdigest()
        source = (ROOT / "scripts/fetch-subc-core.sh").read_text()
        self.assertIn(PIN, source)
        (scripts / "fetch-subc-core.sh").write_text(source.replace(PIN, self.digest))
        shutil.copy(ROOT / "scripts/stage-subc-core-for-runon.sh", scripts)
        for script in scripts.iterdir():
            script.chmod(0o755)
        self.ci = self.root / ".aft-ci" / "subc-core" / TAG
        self.home = self.root / "home"
        self.home.mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        (self.bin / "uname").write_text('#!/bin/sh\ncase "$1" in -s) echo "${TEST_OS:-Linux}";; -m) echo "${TEST_ARCH:-x86_64}";; esac\n')
        (self.bin / "gh").write_text('''#!/bin/bash
set -eu
printf 'called\\n' >> "$GH_LOG"
while [[ "$1" != --dir ]]; do shift; done
cp "$GH_FIXTURE" "$2/ck-subc-linux-x64.zip"
''')
        for executable in self.bin.iterdir():
            executable.chmod(0o755)
        self.log = self.root / "gh.log"
        self.env = dict(os.environ, HOME=str(self.home), TMPDIR=str(self.root),
                        PATH=f"{self.bin}{os.pathsep}{os.environ['PATH']}",
                        GH_LOG=str(self.log), GH_FIXTURE=str(self.fixture))

    def run_script(self, name="fetch-subc-core.sh", **env):
        return subprocess.run([str(self.root / "scripts" / name)], text=True,
                              capture_output=True, env={**self.env, **env})

    def stage_fixture(self):
        self.ci.mkdir(parents=True)
        shutil.copy(self.fixture, self.ci / ARCHIVE)
        (self.ci / f"{ARCHIVE}.sha256").write_text(self.digest + "\n")

    def test_staged_archive_is_used_before_cache_without_gh(self):
        self.stage_fixture()
        cache = self.home / ".cache/aft-ci/subc-core" / TAG
        cache.mkdir(parents=True)
        (cache / "ck-subc").write_text("stale binary")
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(Path(result.stdout.strip()).read_text(), "fixture daemon\n")
        self.assertFalse(self.log.exists())

    def test_bad_staged_archive_fails_without_fallback(self):
        self.stage_fixture()
        (self.ci / ARCHIVE).write_bytes(b"corrupt")
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("SHA-256 mismatch", result.stderr)
        self.assertFalse(self.log.exists())

    def test_valid_but_unpinned_staged_archive_fails(self):
        self.stage_fixture()
        (self.ci / ARCHIVE).write_bytes(b"not the pinned archive")
        other = hashlib.sha256((self.ci / ARCHIVE).read_bytes()).hexdigest()
        (self.ci / f"{ARCHIVE}.sha256").write_text(other + "\n")
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match pinned SHA-256", result.stderr)
        self.assertFalse(self.log.exists())

    def test_partial_staged_archive_fails_without_fallback(self):
        self.stage_fixture()
        (self.ci / f"{ARCHIVE}.sha256").unlink()
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Incomplete staged", result.stderr)
        self.assertFalse(self.log.exists())

    def test_absent_archive_uses_existing_download_path(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.log.read_text(), "called\n")
        self.assertEqual(Path(result.stdout.strip()).read_text(), "fixture daemon\n")
        self.assertFalse(self.ci.exists())

    def test_stage_fetches_linux_on_darwin_and_reuses_cache(self):
        host_cache = self.home / ".cache/aft-ci/subc-core" / TAG
        host_cache.mkdir(parents=True)
        (host_cache / "ck-subc").write_text("host daemon")
        result = self.run_script("stage-subc-core-for-runon.sh", TEST_OS="Darwin", TEST_ARCH="arm64")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("untracked on purpose", result.stdout)
        self.assertEqual(len(result.stdout.splitlines()), 1)
        self.assertEqual((self.ci / ARCHIVE).read_bytes(), self.fixture.read_bytes())
        self.assertEqual((self.ci / f"{ARCHIVE}.sha256").read_text().strip(), self.digest)
        self.assertEqual((host_cache / "ck-subc").read_text(), "host daemon")
        self.assertEqual(self.log.read_text(), "called\n")
        result = self.run_script("stage-subc-core-for-runon.sh", TEST_OS="Darwin", TEST_ARCH="arm64")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.log.read_text(), "called\n")


if __name__ == "__main__":
    unittest.main()
