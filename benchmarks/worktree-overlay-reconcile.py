#!/usr/bin/env python3
"""Reproduce the linked-worktree RAM-overlay search gaps (issues #337, #338).

A linked worktree with `worktree.ram_overlay: true` borrows the home checkout's
shared trigram `cache.bin` read-only and layers watcher events on top in RAM.
This harness checks two ways that overlay can miss content:

* startup: everything that already differs from the shared snapshot when the
  worktree's AFT process starts (branch commits, new files, uncommitted edits,
  untracked files) must be searchable once search reports `Ready`;
* rescan: after a watcher overflow rescan, an edit made earlier in the session
  must still be searchable once the borrowed snapshot is reloaded.

Every AFT process runs with an isolated `AFT_STORAGE_DIR`, HOME, and XDG roots.
The rescan phase forces an FSEvents overflow by stopping the process, touching
many files, and resuming it. The overflow is not guaranteed on every attempt,
so the harness retries and reports whether a rescan was observed in the log.

Usage:
    python3 benchmarks/worktree-overlay-reconcile.py --binary target/debug/aft --rescan

    # Reconciliation cost on a clone of a real repository:
    python3 benchmarks/worktree-overlay-reconcile.py --binary target/stage/aft \
        --source-repo . --worktree-ref HEAD~200
"""

from __future__ import annotations

import argparse
import json
import os
import select
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

JsonObject = dict[str, Any]

STARTUP_TOKENS = [
    ("HOME_BASE_TOKEN", "unchanged, same as home"),
    ("BRANCH_EDITED_TOKEN", "line added in a branch commit"),
    ("BRANCH_NEW_FILE_TOKEN", "new file in a branch commit"),
    ("UNCOMMITTED_EDIT_TOKEN", "uncommitted edit made before AFT started"),
    ("UNTRACKED_TOKEN", "untracked file"),
]


class AftClient:
    def __init__(self, binary: Path, env: dict[str, str], stderr_path: Path) -> None:
        self.stderr_path = stderr_path
        self.stderr_file = stderr_path.open("wb")
        self.process = subprocess.Popen(
            [str(binary)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr_file,
            env=env,
            bufsize=0,
        )
        self.buffer = b""
        self.next_id = 0

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=10)
        self.stderr_file.close()

    def call(self, command: str, timeout_secs: float = 120.0, **params: Any) -> JsonObject:
        self.next_id += 1
        request_id = str(self.next_id)
        request = {"id": request_id, "command": command, **params}
        assert self.process.stdin is not None and self.process.stdout is not None
        self.process.stdin.write((json.dumps(request) + "\n").encode())
        self.process.stdin.flush()
        deadline = time.time() + timeout_secs
        while time.time() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"AFT exited with code {self.process.returncode}")
            ready, _, _ = select.select([self.process.stdout], [], [], 0.1)
            if ready:
                chunk = os.read(self.process.stdout.fileno(), 65_536)
                if chunk:
                    self.buffer += chunk
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                try:
                    frame = json.loads(line)
                except (json.JSONDecodeError, UnicodeDecodeError):
                    continue
                if frame.get("id") == request_id:
                    return frame
        raise TimeoutError(f"timed out waiting for {command}")

    def log_text(self) -> str:
        self.stderr_file.flush()
        return self.stderr_path.read_text(errors="replace")


def run(*args: str, cwd: Path | None = None) -> None:
    subprocess.run(args, cwd=cwd, check=True)


def isolated_env(base: Path) -> dict[str, str]:
    env = dict(os.environ)
    for name in ("home", "xdg-config", "xdg-data", "xdg-cache", "xdg-state", "storage"):
        (base / name).mkdir(parents=True, exist_ok=True)
    env.update(
        {
            "HOME": str(base / "home"),
            "XDG_CONFIG_HOME": str(base / "xdg-config"),
            "XDG_DATA_HOME": str(base / "xdg-data"),
            "XDG_CACHE_HOME": str(base / "xdg-cache"),
            "XDG_STATE_HOME": str(base / "xdg-state"),
            "AFT_STORAGE_DIR": str(base / "storage"),
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_AUTHOR_NAME": "repro",
            "GIT_AUTHOR_EMAIL": "repro@example.invalid",
            "GIT_COMMITTER_NAME": "repro",
            "GIT_COMMITTER_EMAIL": "repro@example.invalid",
        }
    )
    return env


def build_fixture(base: Path, env: dict[str, str], filler: int) -> tuple[Path, Path]:
    home = base / "home-checkout"
    worktree = base / "wt"
    home.mkdir()

    def git(*args: str, cwd: Path) -> None:
        subprocess.run(("git",) + args, cwd=cwd, check=True, env=env)

    git("init", "-q", "-b", "main", cwd=home)
    (home / "a.ts").write_text("export const HOME_BASE_TOKEN = 1;\n")
    (home / "u.ts").write_text("export const UNCHANGED = 1;\n")
    filler_dir = home / "filler"
    filler_dir.mkdir()
    for index in range(filler):
        (filler_dir / f"f{index:05d}.ts").write_text(f"export const FILLER_{index} = {index};\n")
    git("add", ".", cwd=home)
    git("commit", "-qm", "base", cwd=home)
    git("worktree", "add", "-q", "-b", "feat", str(worktree), cwd=home)
    with (worktree / "a.ts").open("a") as handle:
        handle.write("export const BRANCH_EDITED_TOKEN = 2;\n")
    (worktree / "b.ts").write_text("export const BRANCH_NEW_FILE_TOKEN = 3;\n")
    git("add", ".", cwd=worktree)
    git("commit", "-qm", "branch work", cwd=worktree)
    (worktree / "u.ts").write_text("export const UNCOMMITTED_EDIT_TOKEN = 5;\n")
    (worktree / "d.ts").write_text("export const UNTRACKED_TOKEN = 6;\n")
    return home, worktree


def configure(client: AftClient, root: Path, storage: Path, ram_overlay: bool) -> JsonObject:
    doc = {
        "search_index": True,
        "semantic_search": False,
        "callgraph_store": False,
        "inspect": {"enabled": False},
        "worktree": {"ram_overlay": ram_overlay},
    }
    response = client.call(
        "configure",
        project_root=str(root),
        harness="opencode",
        storage_dir=str(storage),
        config=[{"tier": "user", "source": str(storage / "aft.jsonc"), "doc": json.dumps(doc)}],
    )
    if not response.get("success"):
        raise RuntimeError(f"configure failed: {response}")
    return response


def grep(client: AftClient, pattern: str) -> tuple[int, str]:
    response = client.call("grep", pattern=pattern)
    if not response.get("success", True) and "total_matches" not in response:
        raise RuntimeError(f"grep failed: {response}")
    return int(response.get("total_matches", 0)), str(response.get("index_status"))


def wait_ready(client: AftClient, probe: str, timeout: float = 120.0) -> float:
    started = time.time()
    while time.time() - started < timeout:
        _, status = grep(client, probe)
        if status == "Ready":
            return time.time() - started
        time.sleep(0.1)
    raise TimeoutError("search never reported Ready")


def startup_phase(args: argparse.Namespace, base: Path, env: dict[str, str]) -> list[JsonObject]:
    home, worktree = build_fixture(base, env, args.filler)
    storage = base / "storage"

    owner = AftClient(args.binary, env, base / "owner.log")
    try:
        configure(owner, home, storage, ram_overlay=False)
        wait_ready(owner, "HOME_BASE_TOKEN")
        # Give the owner time to persist the shared snapshot, then stop it.
        time.sleep(2.0)
    finally:
        owner.close()

    borrower = AftClient(args.binary, env, base / "worktree.log")
    rows: list[JsonObject] = []
    try:
        response = configure(borrower, worktree, storage, ram_overlay=True)
        ready_after = wait_ready(borrower, "HOME_BASE_TOKEN")
        time.sleep(2.0)
        for token, where in STARTUP_TOKENS:
            matches, status = grep(borrower, token)
            rows.append({"token": token, "where": where, "matches": matches, "index_status": status})
        rows.append({"ready_after_s": round(ready_after, 3), "artifact_owner": response.get("artifact_owner")})
        if args.rescan:
            rows.extend(rescan_phase(args, borrower, worktree))
    finally:
        borrower.close()
    return rows


def rescan_phase(args: argparse.Namespace, client: AftClient, worktree: Path) -> list[JsonObject]:
    rows: list[JsonObject] = []
    edited = worktree / "edited-this-session.ts"
    edited.write_text("export const EDITED_THIS_SESSION_TOKEN = 7;\n")
    deadline = time.time() + 30
    while time.time() < deadline:
        matches, status = grep(client, "EDITED_THIS_SESSION_TOKEN")
        if matches == 1 and status == "Ready":
            break
        time.sleep(0.2)
    rows.append({"step": "after edit", "matches": matches, "index_status": status})

    touch_dir = worktree / "touched"
    touch_dir.mkdir(exist_ok=True)
    rescans_before = client.log_text().count("kind=watcher_rescan")
    observed = False
    for attempt in range(args.rescan_attempts):
        client.process.send_signal(signal.SIGSTOP)
        try:
            for index in range(args.touch_count):
                path = touch_dir / f"t{index:05d}.txt"
                path.write_text(f"touch {attempt} {index}\n")
        finally:
            client.process.send_signal(signal.SIGCONT)
        time.sleep(3.0)
        if client.log_text().count("kind=watcher_rescan") > rescans_before:
            observed = True
            break
    rows.append({"step": "rescan observed", "value": observed, "attempts": attempt + 1})

    matches, status = grep(client, "EDITED_THIS_SESSION_TOKEN")
    rows.append({"step": "first search after rescan", "matches": matches, "index_status": status})
    deadline = time.time() + 30
    while time.time() < deadline:
        matches, status = grep(client, "EDITED_THIS_SESSION_TOKEN")
        if status == "Ready":
            break
        time.sleep(0.2)
    rows.append({"step": "after borrowed reload", "matches": matches, "index_status": status})
    time.sleep(5.0)
    matches, status = grep(client, "EDITED_THIS_SESSION_TOKEN")
    rows.append({"step": "5 seconds later", "matches": matches, "index_status": status})
    return rows


def cost_phase(args: argparse.Namespace, base: Path, env: dict[str, str]) -> list[JsonObject]:
    """Time reconciliation on a real repository instead of the tiny fixture.

    The source repository is cloned as the home checkout, which publishes the
    shared snapshot. A linked worktree is then added at `--worktree-ref`; a
    fresh checkout gives every file a new mtime, so this is the case where
    every same-size file must be content-hashed.
    """
    home = base / "home-checkout"
    worktree = base / "wt"
    storage = base / "storage"
    subprocess.run(
        ["git", "clone", "-q", str(args.source_repo), str(home)], check=True, env=env
    )
    subprocess.run(
        ["git", "worktree", "add", "-q", "--detach", str(worktree), args.worktree_ref],
        cwd=home,
        check=True,
        env=env,
    )
    rows: list[JsonObject] = []
    owner = AftClient(args.binary, env, base / "owner.log")
    try:
        configure(owner, home, storage, ram_overlay=False)
        rows.append({"owner_ready_s": round(wait_ready(owner, "fn main", timeout=900), 3)})
        time.sleep(2.0)
    finally:
        owner.close()
    for run_index in range(args.repeats):
        borrower = AftClient(args.binary, env, base / f"worktree-{run_index}.log")
        try:
            configure(borrower, worktree, storage, ram_overlay=True)
            ready = wait_ready(borrower, "fn main", timeout=900)
            line = next(
                (
                    entry
                    for entry in borrower.log_text().splitlines()
                    if "reconciled borrowed snapshot" in entry
                ),
                "",
            )
            rows.append(
                {
                    "run": run_index,
                    "worktree_ready_s": round(ready, 3),
                    "reconcile": line.split(": ", 2)[-1] if line else None,
                }
            )
        finally:
            borrower.close()
    return rows


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--filler", type=int, default=0, help="extra unchanged files in the fixture")
    parser.add_argument("--rescan", action="store_true", help="also run the watcher-rescan phase")
    parser.add_argument("--touch-count", type=int, default=4000)
    parser.add_argument("--rescan-attempts", type=int, default=5)
    parser.add_argument(
        "--source-repo",
        type=Path,
        help="measure reconciliation cost on a clone of this repository instead",
    )
    parser.add_argument("--worktree-ref", default="HEAD")
    parser.add_argument("--repeats", type=int, default=3)
    args = parser.parse_args()
    args.binary = args.binary.resolve()

    base = Path(tempfile.mkdtemp(prefix="aft-overlay-", dir=args.work_dir))
    base = base.resolve()
    env = isolated_env(base)
    phase = cost_phase if args.source_repo else startup_phase
    for row in phase(args, base, env):
        print(json.dumps(row), flush=True)
    print(json.dumps({"work_dir": str(base)}), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
