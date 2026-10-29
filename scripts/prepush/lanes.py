#!/usr/bin/env python3
"""Print transport-free, commit-pinned commands for the mandatory server lanes."""
import json
import os
import shlex
import subprocess
import sys


LANES = (
    ("fast-compile", "linux,4c,net", 4, 2700),
    ("linux-rust", "linux,16c,net", 16, 2700),
    ("linux-typescript", "linux,16c,net", 16, 2700),
    ("linux-search", "linux,16c,net", 16, 5400),
    ("windows", "windows", None, 2700),
    ("macos", "macos", 4, 2700),
)


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()


def plan():
    commit = git("rev-parse", "HEAD")
    base = git("merge-base", os.environ.get("PREPUSH_BASE", "origin/main"), commit)
    branch = os.environ.get("PREPUSH_BRANCH") or git("branch", "--show-current")
    if not branch:
        raise SystemExit("Detached HEAD: set PREPUSH_BRANCH to the reviewed branch name")
    for number, (lane, runon, cpus, seconds) in enumerate(LANES, 1):
        command = shlex.join([
            "env", f"PREPUSH_COMMIT={commit}", f"PREPUSH_BASE={base}",
            f"PREPUSH_BRANCH={branch}", "bash", f"scripts/prepush/{lane}.sh",
        ])
        item = {"number": number, "lane": lane, "command": command, "runon": runon, "vcpus": cpus, "timeout": seconds * 1000}
        if runon.startswith("linux,"):
            item["return_paths"] = [f".cortexkit/internal-docs/investigations/prepush/{lane}.log"]
        yield item


if __name__ == "__main__":
    if sys.argv[1:]:
        raise SystemExit("usage: lanes.py")
    for item in plan():
        print(json.dumps(item))
