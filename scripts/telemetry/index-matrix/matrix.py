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
REVISION = "meaning-nine-v2.1"
POLICY_REVISION = "meaning-nine-v2"
FINISHED = {"ml-explore/mlx", "neovim/neovim", "Perl/perl5", "apache/superset", "ethereum/solidity", "ziglang/zig"}
MEANING = {"microsoft/vscode", "elastic/elasticsearch", "kubernetes/kubernetes"}


def phase_plan(entries):
    by_name = {row["repository"]: row for row in entries}
    def small(names):
        return sorted(names, key=lambda name: (by_name[name]["tracked_files"], name))
    no_meaning = set(by_name) - FINISHED - MEANING
    order = ["NVIDIA/TensorRT-LLM", *small(no_meaning - {"NVIDIA/TensorRT-LLM"}), *small(MEANING), *small(FINISHED)]
    return [{"repository": name,
             "phases": ["callgraph"] if name in FINISHED else (["text", "callgraph", "full"] if name in MEANING else ["text", "callgraph"]),
             "query_build": "none" if name in FINISHED else ("full" if name in MEANING else "callgraph"),
             "meaning_policy": "previous measurements retained" if name in FINISHED else ("full only" if name in MEANING else "off")}
            for name in order]


def now():
    return datetime.now(timezone.utc).isoformat()


def save(path, value):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def drive(manifest_path, results, arguments, revision=REVISION):
    results.mkdir(parents=True, exist_ok=True)
    entries = json.loads(manifest_path.read_text())["repositories"]
    projects = phase_plan(entries)
    identity = {"manifest_sha256": digest(manifest_path), "driver_sha256": digest(Path(__file__)),
                "runner_sha256": digest(HERE / "run.py"), "arguments": arguments, "revision": revision, "projects": projects}
    plan_path = results / "matrix-plan.json"
    previous_plan = json.loads(plan_path.read_text()) if plan_path.exists() else None
    if previous_plan and previous_plan.get("revision") == revision:
        if previous_plan["identity"] != identity:
            raise ValueError("matrix inputs changed within this revision; use a new revision or results directory")
        plan = previous_plan
    else:
        if previous_plan and (previous_plan.get("revision") not in (None, "all-meaning-v1", POLICY_REVISION) or
                              previous_plan["identity"]["manifest_sha256"] != identity["manifest_sha256"] or
                              previous_plan["identity"]["arguments"] != arguments):
            raise ValueError("only explicit unchanged-input v1/v2 to v2.1 transitions are supported")
        if previous_plan and previous_plan.get("revision") == POLICY_REVISION and previous_plan.get("projects") != projects:
            raise ValueError("v2.1 may not change the nine-meaning phase policy")
        plan = {"started_at": previous_plan["started_at"] if previous_plan else now(),
                "revised_at": now(), "revision": revision, "policy_revision": POLICY_REVISION, "identity": identity,
                "order": [row["repository"] for row in projects], "projects": projects,
                "previous_plans": [previous_plan] if previous_plan else [],
                "retired_partial_attempts": previous_plan.get("retired_partial_attempts", []) if previous_plan else [],
                "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=HERE, text=True).strip()}
        save(plan_path, plan)
    state_path = results / "matrix-state.json"
    state = json.loads(state_path.read_text()) if state_path.exists() else {"started_at": plan["started_at"], "repositories": {}}
    if state.get("revision") != revision:
        state = {"started_at": plan["started_at"], "revision": revision, "revised_at": plan["revised_at"],
                 "previous_states": [state], "repositories": {row["repository"]: {"status": "pending", "phase_plan": row["phases"],
                                                                                 "query_build": row["query_build"]} for row in projects}}
        save(state_path, state)
    state.pop("paused_reason", None)
    state.pop("finished_at", None)
    for entry in projects:
        if shutil.disk_usage(results).free < 20 * 1024 ** 3:
            state["paused_reason"] = "less than 20 GiB free before next project; free disk space, then resume"
            save(state_path, state)
            return 75
        name = entry["repository"]
        directory = results / name.replace("/", "--")
        directory.mkdir(exist_ok=True)
        command = [sys.executable, str(HERE / "run.py"), "--manifest", str(manifest_path),
                   "--results", str(results), "--repository", name, "--plan-revision", revision,
                   "--phases", ",".join(entry["phases"]), "--query-build", entry["query_build"], *arguments]
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
                    "callgraph_outcome": result.get("callgraph", {}).get("outcome"),
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
    parser.add_argument("--revision", choices=(REVISION,), default=REVISION)
    parser.add_argument("arguments", nargs=argparse.REMAINDER, help="after --, arguments passed unchanged to run.py")
    args = parser.parse_args()
    arguments = args.arguments[1:] if args.arguments and args.arguments[0] == "--" else args.arguments
    if any(item in arguments for item in ("--repository", "--results", "--manifest", "--plan-revision", "--phases", "--query-build")):
        parser.error("repository, results and manifest are owned by the matrix driver")
    args.results = args.results.resolve()
    args.results.mkdir(parents=True, exist_ok=True)
    with (args.results / ".driver.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return drive(args.manifest.resolve(), args.results, arguments, args.revision)


if __name__ == "__main__":
    raise SystemExit(main())
