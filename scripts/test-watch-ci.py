#!/usr/bin/env python3
"""Live CLI regression tests for watch-ci.sh (no GitHub mutations).

Run with WATCH_CI_LIVE_TESTS=1 python3 scripts/test-watch-ci.py.
Requires upstream gh authentication and a successful recent CI run whose commit
is present locally. WATCH_CI_WORKFLOW can select the workflow; otherwise the suite
uses tests.yml or ci.yml, whichever exists in this checkout. REPO can override the
GitHub repository inferred from origin. Tests create only disposable local refs.

Two of the five cases discriminate the resolver defect (an abbreviated sha and an
annotated tag both go red when the argument is matched by shape instead of
resolved through git); the other three pass under both shapes on purpose - they
pin the paths that must not change while the resolver is edited (full sha, run
id, unknown ref refusing fast), so a "2 of 5 red" mutation result is the expected
reading, not a sign the rest are decoration.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

SCRIPT = Path(__file__).resolve().with_name("watch-ci.sh")
ROOT = SCRIPT.parent.parent
LIVE = os.environ.get("WATCH_CI_LIVE_TESTS") == "1"


def run(*args: str, cwd: Path = ROOT, env: dict[str, str] | None = None) -> str:
    return subprocess.run(
        args, cwd=cwd, env=env, text=True, capture_output=True, check=True, timeout=60,
    ).stdout.strip()


@unittest.skipUnless(LIVE, "live GitHub tests disabled; set WATCH_CI_LIVE_TESTS=1")
class WatchCiLiveTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.git = shutil.which("git")
        if cls.git is None:
            raise RuntimeError("git is required for WATCH_CI_LIVE_TESTS=1")
        cls.env = os.environ.copy()
        # Host repository overrides must not redirect operations out of the
        # disposable clone. Author settings make annotated tags independent of
        # the operator's signing and identity configuration.
        for key in list(cls.env):
            if key.startswith("GIT_"):
                cls.env.pop(key)
        cls.env.update({
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_AUTHOR_NAME": "Watcher test",
            "GIT_AUTHOR_EMAIL": "watcher@example.invalid",
            "GIT_COMMITTER_NAME": "Watcher test",
            "GIT_COMMITTER_EMAIL": "watcher@example.invalid",
            "GIT_TERMINAL_PROMPT": "0",
        })
        repo = cls.env.get("REPO")
        if not repo:
            origin = run(cls.git, "config", "--get", "remote.origin.url", env=cls.env)
            match = re.fullmatch(r"(?:https://github\.com/|git@github\.com:|ssh://git@github\.com/)([^/]+/[^/]+)", origin)
            if match is None:
                raise RuntimeError("cannot infer GitHub repo from origin; set REPO=owner/name")
            repo = match[1].removesuffix(".git")
        workflow = cls.env.get("WATCH_CI_WORKFLOW")
        if not workflow:
            workflow = next((name for name in ("tests.yml", "ci.yml")
                             if (ROOT / ".github/workflows" / name).is_file()), None)
        if not workflow:
            raise RuntimeError("set WATCH_CI_WORKFLOW to the workflow being tested")
        cls.env.update({
            "REPO": repo,
            "WATCH_CI_WORKFLOW": workflow,
            "WATCH_CI_RESOLVE_ATTEMPTS": "2",
            "WATCH_CI_RESOLVE_SLEEP": "0",
            "WATCH_CI_SETTLE": "0",
        })
        # Locate the real GitHub CLI using the shipped selector, avoiding command-
        # routing wrappers installed for AI tools. Exercise the watcher itself
        # only by spawning its command-line entrypoint.
        cls.gh = run("bash", "-c", 'source "$1" && printf "%s" "$OPERATOR_GH"',
                     "bash", str(SCRIPT.parent / "lib/operator-gh.sh"), env=cls.env)
        rows = json.loads(run(
            cls.gh, "run", "list", "--repo", repo, "--workflow", workflow,
            "--limit", "40", "--json", "databaseId,headSha,status,conclusion", env=cls.env,
        ))
        seen = set()
        for row in rows:
            sha = row["headSha"]
            if sha in seen:
                continue
            seen.add(sha)
            if row["status"] != "completed" or row["conclusion"] != "success":
                continue
            local = subprocess.run(
                [cls.git, "rev-parse", "--verify", sha + "^{commit}"],
                cwd=ROOT, env=cls.env, capture_output=True, text=True, timeout=10,
            )
            if local.returncode == 0 and local.stdout.strip() == sha:
                cls.sha = sha
                cls.run_id = str(row["databaseId"])
                break
        else:
            raise RuntimeError("no recent successful CI run has a locally available commit; fetch the tested commits and retry")
        print(f"Live fixture: repo={repo} workflow={workflow} commit={cls.sha} run={cls.run_id}", flush=True)

    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix=".watch-ci-live-", dir=ROOT)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        self.env = self.__class__.env.copy()
        run(self.git, "clone", "--quiet", "--shared", "--no-checkout", str(ROOT), str(self.repo), env=self.env)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "gh-calls.jsonl"
        self.env["PATH"] = str(self.bin) + os.pathsep + self.env["PATH"]
        # Log invocations, then execute real gh unchanged. This measures polling
        # without depending on the watcher's jq expression or resolver internals.
        wrapper = self.bin / "gh"
        wrapper.write_text(
            "#!" + sys.executable + "\nimport json, os, sys\n"
            + f"with open({str(self.log)!r}, 'a') as log:\n"
            + "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            + f"os.execv({self.gh!r}, [{self.gh!r}, *sys.argv[1:]])\n"
        )
        wrapper.chmod(0o755)

    def watch(self, argument: str, timeout: float = 60) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["bash", str(SCRIPT), argument], cwd=self.repo, env=self.env,
            text=True, capture_output=True, timeout=timeout,
        )

    def calls(self) -> list[list[str]]:
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def found_run(self, result: subprocess.CompletedProcess[str]) -> str:
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        match = re.search(r"watching run (\d+)", result.stdout)
        self.assertIsNotNone(match, result.stdout)
        self.assertIn("conclusion=success", result.stdout)
        self.assertTrue(self.calls(), "gh invocation logger must observe real calls")
        return match[1]

    def test_full_sha_resolves_and_finds_run(self) -> None:
        self.assertEqual(self.found_run(self.watch(self.sha)), self.run_id)

    def test_abbreviated_sha_resolves_to_same_run_as_full_sha(self) -> None:
        short = run(self.git, "rev-parse", "--short=8", self.sha, cwd=self.repo, env=self.env)
        self.assertLess(len(short), len(self.sha))
        full_run = self.found_run(self.watch(self.sha))
        self.assertEqual(self.found_run(self.watch(short)), full_run)
        self.assertEqual(full_run, self.run_id)

    def test_unknown_ref_refuses_fast_without_polling(self) -> None:
        unknown = "missing-ref-" + self.root.name
        self.env["WATCH_CI_RESOLVE_SLEEP"] = "60"
        started = time.monotonic()
        result = self.watch(unknown, timeout=5)
        elapsed = time.monotonic() - started
        self.assertLess(elapsed, 5, "unknown refs must not consume the polling budget")
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn(unknown, result.stdout + result.stderr)
        self.assertEqual(self.calls(), [], "unknown ref must refuse before invoking gh")

    def test_annotated_tag_resolves_to_commit_run(self) -> None:
        run(self.git, "-c", "tag.gpgsign=false", "tag", "-a", "watcher-test-tag",
            self.sha, "-m", "Disposable watcher test", cwd=self.repo, env=self.env)
        tag_object = run(self.git, "rev-parse", "watcher-test-tag", cwd=self.repo, env=self.env)
        self.assertNotEqual(tag_object, self.sha, "fixture must be annotated, not lightweight")
        self.assertEqual(self.found_run(self.watch("watcher-test-tag")), self.run_id)

    def test_numeric_run_id_bypasses_resolution_entirely(self) -> None:
        git_log = self.root / "git-called"
        blocker = self.bin / "git"
        blocker.write_text("#!/bin/sh\nprintf called > " + shlex.quote(str(git_log)) + "\nexit 91\n")
        blocker.chmod(0o755)
        self.assertEqual(self.found_run(self.watch(self.run_id)), self.run_id)
        self.assertFalse(git_log.exists(), "numeric run IDs must not invoke git")
        self.assertTrue(all(call[:2] == ["run", "view"] for call in self.calls()))


if __name__ == "__main__":
    # EXIT 2 WHEN UNARMED, rather than running unittest and reporting "OK (skipped=5)".
    #
    # A suite whose subject is FALSE PASSES must not have one as its own default. `OK` after
    # zero executed tests is the exact shape this file exists to catch: a reassuring word over
    # an empty measurement. Exit 2 is "cannot compare" -- distinct from 0 (verified) and from
    # 1 (a real failure) -- and it matches the convention already used by this repository's
    # other checkers (wal-verify, live-runs, find-finalize-leaks) for the same reason.
    #
    # Arming requires network and a GitHub token, so unarmed is the COMMON case in a fresh
    # checkout. That is precisely why the unarmed message has to be unmistakable.
    if not LIVE:
        print(
            "CANNOT COMPARE: this suite verified NOTHING.\n"
            "  It exercises watch-ci.sh against real GitHub API responses, so it needs\n"
            "  network and an authenticated gh. Arm it with:\n"
            "      WATCH_CI_LIVE_TESTS=1 python3 scripts/test-watch-ci.py\n"
            "  Run it from the repository (ROOT is derived from this file's location).",
            flush=True,
        )
        raise SystemExit(2)
    unittest.main(verbosity=2)
