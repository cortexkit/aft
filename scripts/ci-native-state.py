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
from typing import Union


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
    # Native E2E injects the built executable into the plugin's versioned cache.
    # Those copies can hold files open after their test harness has exited.
    if os.name == "nt":
        cache = os.environ.get("LOCALAPPDATA") or os.environ.get("APPDATA")
        if cache:
            roots.append(Path(cache) / "aft" / "bin")
    else:
        roots.append(Path.home() / ".cache" / "aft" / "bin")
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


def windows_git_path_entries(exec_path: Path) -> list[Path]:
    for candidate in (exec_path, *exec_path.parents):
        if (candidate / "cmd/git.exe").is_file() or (candidate / "bin/bash.exe").is_file():
            entries = [candidate / part for part in ("bin", "cmd", "ucrt64/bin", "usr/bin")]
            missing = [str(path) for path in entries if not path.is_dir()]
            if missing:
                raise RuntimeError("Git for Windows PATH directories missing: " + ", ".join(missing))
            return entries
    raise RuntimeError(f"Cannot locate Git for Windows installation from {exec_path}")


def publish_windows_git_path(exec_path: Path, path_file: Path) -> None:
    entries = windows_git_path_entries(exec_path)
    # GitHub prepends the last file entry first. Keep the hosted Git directory
    # order so PowerShell can resolve coreutils without forcing another shell.
    with path_file.open("a", encoding="utf-8") as paths:
        for entry in reversed(entries):
            print(entry.as_posix(), file=paths)
    print("Git for Windows PATH order: " + ";".join(entry.as_posix() for entry in entries))


def windows_rust_environment(shared_cargo: Path, rustup: Path, writable_cargo: Path) -> dict[str, Path]:
    toolchain = rustup / "toolchains/stable-x86_64-pc-windows-msvc"
    required = [shared_cargo, rustup, toolchain]
    required += [shared_cargo / "bin" / f"{name}.exe" for name in (
        "cargo", "rustc", "rustdoc", "rustup", "rustfmt", "cargo-clippy", "rust-analyzer",
    )]
    required += [toolchain / "bin" / f"{name}.exe" for name in ("cargo", "rustc", "rustdoc")]
    required.append(toolchain / "lib/rustlib/src/rust/library/core/src/lib.rs")
    missing = [str(path) for path in required if not path.exists()]
    if missing:
        raise RuntimeError("Read-only Rust installation is incomplete; ask the runner administrator to provision: "
                           + ", ".join(missing))
    if (writable_cargo.resolve().is_relative_to(shared_cargo.resolve())
            or writable_cargo.resolve().is_relative_to(rustup.parent.resolve())):
        raise RuntimeError(f"Cargo's writable home must be outside the shared Rust installation: {writable_cargo}")
    (writable_cargo / "bin").mkdir(parents=True, exist_ok=True)
    print(f"Read-only Rust shims: {shared_cargo / 'bin'}; toolchain: {toolchain}")
    return {"CARGO_HOME": writable_cargo}


def setup_windows_tool_paths() -> dict[str, Union[Path, str]]:
    path_file = Path(os.environ["GITHUB_PATH"])
    exec_path = Path(subprocess.check_output(["git", "--exec-path"], text=True).strip())
    publish_windows_git_path(exec_path, path_file)
    shared_cargo = Path(os.environ.get("CARGO_HOME", "C:/rust/cargo"))
    rustup_home = os.environ.get("RUSTUP_HOME")
    if not rustup_home:
        raise RuntimeError("The runner must set RUSTUP_HOME to its administrator-managed Rust installation")
    rustup = Path(rustup_home)
    writable_cargo = Path.home() / ".cargo"
    values: dict[str, Union[Path, str]] = dict(windows_rust_environment(shared_cargo, rustup, writable_cargo))
    values["RUSTUP_TOOLCHAIN"] = "stable-x86_64-pc-windows-msvc"
    # Missing toolchains must fail instead of starting rustup's automatic installer.
    values["RUSTUP_AUTO_INSTALL"] = "0"
    with path_file.open("a", encoding="utf-8") as paths:
        for directory in (shared_cargo / "bin", writable_cargo / "bin"):
            print(directory.as_posix(), file=paths)
    return values


def prune_run_archives(root: Path, run_dir: Path) -> None:
    # Cancelled runs may never finalize. Bound archive retention without ever
    # traversing Cargo's shared target directory.
    before = shutil.disk_usage(root).free
    low_disk = before < 40 * 1024**3
    now = time.time()
    archives = sorted(
        (entry for entry in (root / "runs").iterdir()
         if entry != run_dir and not entry.is_symlink() and entry.is_dir()),
        key=lambda entry: (entry.stat().st_mtime, entry.name),
    )
    if low_disk:
        print(f"CI archive disk free before pruning: {before / 1024**3:.1f} GiB", flush=True)
    for old in archives:
        if low_disk or now - old.stat().st_mtime > 86400:
            print(f"Removing CI run archive {old}", flush=True)
            shutil.rmtree(old)
    if low_disk:
        after = shutil.disk_usage(root).free
        print(f"CI archive disk free after pruning: {after / 1024**3:.1f} GiB", flush=True)


def target_bytes(target: Path) -> int:
    total = 0
    seen: set[tuple[int, int]] = set()
    for directory, children, files in os.walk(target, followlinks=False):
        children[:] = [name for name in children if not (Path(directory) / name).is_symlink()]
        for name in files:
            path = Path(directory) / name
            try:
                if path.is_symlink():
                    continue
                stat = path.stat()
            except FileNotFoundError:
                continue
            identity = (stat.st_dev, stat.st_ino)
            if identity in seen:
                continue
            seen.add(identity)
            total += stat.st_blocks * 512 if hasattr(stat, "st_blocks") else stat.st_size
    return total


def clean_oversized_target(target: Path) -> None:
    if os.environ.get("RUNNER_ENVIRONMENT") != "self-hosted":
        return
    size = target_bytes(target)
    if size > 50 * 1024**3:
        print(f"CI shared target size before cleaning: {size / 1024**3:.1f} GiB ({size} bytes)", flush=True)
        # Cargo owns the dependency graph and fingerprints. Removing the whole
        # target is safe; deleting apparently old deps individually is not.
        # This runs before the toolchain is on the job's PATH, and `cargo clean`
        # refuses a target that has no CACHEDIR.TAG, so remove the folder
        # directly. It is this state root's own `target`, never a caller path.
        shutil.rmtree(target, onexc=_clear_read_only_and_retry)


def _clear_read_only_and_retry(function, path, _error) -> None:
    # Windows refuses to delete read-only files, which cargo and git leave behind.
    os.chmod(path, 0o700)
    function(path)


def setup(root: Path, run_dir: Path) -> None:
    target = root / "target"
    target.mkdir(parents=True, exist_ok=True)
    clean_oversized_target(target)
    target.mkdir(parents=True, exist_ok=True)
    run_dir.mkdir(parents=True, exist_ok=True)
    attempt = os.environ.get("GITHUB_RUN_ATTEMPT", "1")
    job_dir = Path(tempfile.mkdtemp(prefix=f"attempt-{attempt}-job-", dir=run_dir))
    values: dict[str, Union[Path, str]] = {
        "AFT_CI_TARGET_DIR": target,
        "AFT_CI_ARCHIVE_DIR": run_dir,
        "AFT_CI_JOB_DIR": job_dir,
        "AFT_GATE_HOME_ROOT": job_dir / "gate-home",
    }
    if os.name == "nt":
        values.update(setup_windows_tool_paths())
    with open(os.environ["GITHUB_ENV"], "a", encoding="utf-8") as env:
        for key, value in values.items():
            text = value.as_posix() if isinstance(value, Path) else value
            print(f"{key}={text}", file=env)
            print(f"{key}={text}")
    prune_run_archives(root, run_dir)


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
