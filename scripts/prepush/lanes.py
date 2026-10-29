#!/usr/bin/env python3
"""Print transport-free, commit-pinned commands for the mandatory server lanes."""
import json
import os
import shlex
import subprocess
import sys


LANES = (
    # Fast-compile and Rust use caches only. The later TypeScript/Docker and
    # search steps fetch images, packages and pinned corpora, so this single
    # job needs net: network access is granted per job, not per shell step.
    # One workspace runs one job at a time. Run all Linux phases sequentially
    # in one allocation, including fast-compile in the 16-vCPU total.
    ("linux", "linux,16c,net", 16, 7200),
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
            item["return_paths"] = [f".cortexkit/internal-docs/investigations/prepush/{name}.log" for name in ("linux", "fast-compile", "linux-rust", "linux-typescript", "linux-search")]
        yield item


if __name__ == "__main__":
    if sys.argv[1:]:
        raise SystemExit("usage: lanes.py")
    for item in plan():
        print(json.dumps(item))
