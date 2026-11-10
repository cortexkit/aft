#!/usr/bin/env python3
"""Manage local build reuse and job isolation on the dedicated native CI runners."""

import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time


def run_directory(root: Path, run_id: str, attempt: str) -> Path:
    if not re.fullmatch(r"[0-9]+", run_id) or not re.fullmatch(r"[0-9]+", attempt):
        raise ValueError("CI state requires a numeric workflow run ID and attempt")
    # Failed-job retries reuse successful producers from the same exact-SHA run.
    return root / "runs" / run_id


def process_table() -> dict[int, tuple[int, str]]:
    if os.name == "nt":
        result = subprocess.run(
            ["pwsh", "-NoProfile", "-Command",
             "Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,ExecutablePath | ConvertTo-Json -Compress"],
            check=True, capture_output=True, text=True,
        )
        return {p["ProcessId"]: (p["ParentProcessId"], p["ExecutablePath"] or "")
                for p in json.loads(result.stdout)}
    result = subprocess.run(["ps", "-axo", "pid=,ppid=,comm="],
                            check=True, capture_output=True, text=True)
    return {int(pid): (int(parent), executable)
            for line in result.stdout.splitlines()
            for pid, parent, executable in [line.strip().split(None, 2)]}


def leaked_processes(table: dict[int, tuple[int, str]], roots: list[Path], own_pid: int) -> set[int]:
    protected = set()
    current = own_pid
    while current and current not in protected:
        protected.add(current)
        current = table.get(current, (0, ""))[0]
    prefixes = [str(p).replace("\\", "/").lower().rstrip("/") + "/" for p in roots]
    victims = {pid for pid, (_, executable) in table.items()
               if any(executable.replace("\\", "/").lower().startswith(prefix) for prefix in prefixes)}
    # Test helpers can outlive their harness; terminate their entire descendant tree.
    while True:
        children = {pid for pid, (parent, _) in table.items() if parent in victims}
        expanded = victims | children
        if expanded == victims:
            return victims - protected
        victims = expanded


def cleanup_processes(root: Path) -> None:
    roots = [root, Path(os.environ["GITHUB_WORKSPACE"]) / "target"]
    victims = leaked_processes(process_table(), roots, os.getpid())
    for pid in sorted(victims, reverse=True):
        print(f"Stopping leaked native CI process {pid}", flush=True)
        if os.name == "nt":
            subprocess.run(["taskkill", "/T", "/F", "/PID", str(pid)],
                           capture_output=True, check=False)
        else:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


def setup(root: Path, run_dir: Path) -> None:
    target = root / "target"
    target.mkdir(parents=True, exist_ok=True)
    run_dir.mkdir(parents=True, exist_ok=True)
    attempt = os.environ.get("GITHUB_RUN_ATTEMPT", "1")
    job_dir = Path(tempfile.mkdtemp(prefix=f"attempt-{attempt}-job-", dir=run_dir))
    values = {
        "CARGO_TARGET_DIR": target,
        "RUSTC_WRAPPER": "",
        "SCCACHE_GHA_ENABLED": "false",
        "AFT_CI_ARCHIVE_DIR": run_dir,
        "AFT_CI_JOB_DIR": job_dir,
        "AFT_GATE_HOME_ROOT": job_dir / "gate-home",
    }
    with open(os.environ["GITHUB_ENV"], "a", encoding="utf-8") as env:
        for key, value in values.items():
            text = value.as_posix() if isinstance(value, Path) else value
            print(f"{key}={text}", file=env)
            print(f"{key}={text}")
    # Cancelled runs may never reach finalization. Keep a week's grace for queued
    # consumers, pruning only archives, never Cargo's shared build directory.
    for old in (root / "runs").iterdir():
        if old != run_dir and time.time() - old.stat().st_mtime > 7 * 86400:
            shutil.rmtree(old)


def main() -> None:
    phase = sys.argv[1]
    if os.environ.get("RUNNER_ENVIRONMENT") != "self-hosted":
        if phase == "setup":
            with open(os.environ["GITHUB_ENV"], "a", encoding="utf-8") as env:
                print(f"AFT_CI_ARCHIVE_DIR={os.environ['RUNNER_TEMP']}", file=env)
        return
    root = Path.home() / "aft-ci"
    run_dir = run_directory(root, os.environ["GITHUB_RUN_ID"], os.environ["GITHUB_RUN_ATTEMPT"])
    cleanup_processes(root)
    if phase == "setup":
        setup(root, run_dir)
    elif phase == "cleanup":
        job = os.environ.get("AFT_CI_JOB_DIR")
        if job and Path(job).parent == run_dir:
            shutil.rmtree(job, ignore_errors=True)
    elif phase == "finalize":
        shutil.rmtree(run_dir, ignore_errors=True)
    else:
        raise ValueError(f"unknown CI state phase: {phase}")


if __name__ == "__main__":
    main()
