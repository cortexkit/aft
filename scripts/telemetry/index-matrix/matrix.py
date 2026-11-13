#!/usr/bin/env python3
"""Durable smallest-first matrix driver; run inside systemd-run or nohup."""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import fcntl
import hashlib
import json
import shutil
import signal
from pathlib import Path
import subprocess
import sys

HERE = Path(__file__).resolve().parent


def now():
    return datetime.now(timezone.utc).isoformat()


def save(path, value):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def ordered(entries):
    # Actual checkout sizes are unavailable until measurement; tracked paths
    # are the manifest's consistent, complete project-size ordering metric.
    return sorted(entries, key=lambda row: (row["repository"] == "chromium/chromium", row["tracked_files"], row["repository"]))


def drive(manifest_path, results, arguments):
    results.mkdir(parents=True, exist_ok=True)
    entries = ordered(json.loads(manifest_path.read_text())["repositories"])
    identity = {"manifest_sha256": digest(manifest_path), "driver_sha256": digest(Path(__file__)),
                "runner_sha256": digest(HERE / "run.py"), "arguments": arguments}
    plan_path = results / "matrix-plan.json"
    if plan_path.exists():
        plan = json.loads(plan_path.read_text())
        if plan["identity"] != identity:
            raise ValueError("matrix inputs changed; use a new results directory")
    else:
        plan = {"started_at": now(), "identity": identity, "order": [row["repository"] for row in entries],
                "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=HERE, text=True).strip()}
        save(plan_path, plan)
    state_path = results / "matrix-state.json"
    state = json.loads(state_path.read_text()) if state_path.exists() else {"started_at": plan["started_at"], "repositories": {}}
    state.pop("paused_reason", None)
    state.pop("finished_at", None)
    for entry in entries:
        if shutil.disk_usage(results).free < 20 * 1024 ** 3:
            state["paused_reason"] = "less than 20 GiB free before next project; free disk space, then resume"
            save(state_path, state)
            return 75
        name = entry["repository"]
        directory = results / name.replace("/", "--")
        directory.mkdir(exist_ok=True)
        command = [sys.executable, str(HERE / "run.py"), "--manifest", str(manifest_path),
                   "--results", str(results), "--repository", name, *arguments]
        previous = state["repositories"].get(name, {})
        state["current_repository"] = name
        state["repositories"][name] = {**previous, "status": "running", "started_at": now(), "command": command}
        save(state_path, state)
        print(f"{now()} running {name}; log={directory / 'run.log'}", flush=True)
        low_disk = False
        with (directory / "run.log").open("a") as output:
            child = subprocess.Popen(command, stdout=output, stderr=subprocess.STDOUT)
            while True:
                try:
                    exit_code = child.wait(timeout=30)
                    break
                except subprocess.TimeoutExpired:
                    if shutil.disk_usage(results).free < 10 * 1024 ** 3:
                        low_disk = True
                        child.send_signal(signal.SIGINT)
                        exit_code = child.wait(timeout=60)
                        break
        result_path = directory / "result.json"
        result = json.loads(result_path.read_text()) if result_path.exists() else {}
        row = state["repositories"][name]
        row.update({"finished_at": now(), "exit_code": exit_code,
                    "status": "completed" if exit_code == 0 and result.get("completed") else "failed",
                    "full_outcome": result.get("full", {}).get("outcome"),
                    "meaning_files_failed": result.get("meaning", {}).get("semantic_failures", {}).get("files_failed"),
                    "full_files_failed": result.get("full", {}).get("semantic_failures", {}).get("files_failed")})
        save(state_path, state)
        subprocess.run([sys.executable, str(HERE / "tables.py"), "--manifest", str(manifest_path), "--results", str(results)], check=True)
        print(f"{now()} finished {name}: {row['status']} full={row['full_outcome']}", flush=True)
        if low_disk:
            state["paused_reason"] = "less than 10 GiB free during project; interrupted safely, free disk space then resume"
            save(state_path, state)
            return 75
    state["current_repository"] = None
    state["finished_at"] = now()
    save(state_path, state)
    return 0 if all(row["status"] == "completed" for row in state["repositories"].values()) else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=HERE / "manifest.json")
    parser.add_argument("--results", type=Path, required=True)
    parser.add_argument("arguments", nargs=argparse.REMAINDER, help="after --, arguments passed unchanged to run.py")
    args = parser.parse_args()
    arguments = args.arguments[1:] if args.arguments and args.arguments[0] == "--" else args.arguments
    if any(item in arguments for item in ("--repository", "--results", "--manifest")):
        parser.error("repository, results and manifest are owned by the matrix driver")
    args.results = args.results.resolve()
    args.results.mkdir(parents=True, exist_ok=True)
    with (args.results / ".driver.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return drive(args.manifest.resolve(), args.results, arguments)


if __name__ == "__main__":
    raise SystemExit(main())
