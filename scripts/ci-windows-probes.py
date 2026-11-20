#!/usr/bin/env python3
"""Run serial Windows probes from the exact library harness built for an archive."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import signal
import subprocess
import tarfile


ROOT = Path(__file__).resolve().parents[1]
PROBES = {
    "tls": "semantic_index::tests::platform_verifier_tls_client_subprocess",
    "backtrace": "logging::tests::panic_hook_preserves_default_backtrace_behavior_and_persists",
}
# nextest gave these probes a 10-minute slow-test kill when it ran them. A probe
# whose spawned trust check hangs must end the step well before the archive
# job's 25-minute limit, or no downstream shard ever receives the archive.
PROBE_TIMEOUT_S = 600
LIST_TIMEOUT_S = 120
TIMEOUT_EXIT = 124


def library_harness(metadata: dict, library_name: str = "aft") -> Path:
    candidates = [
        binary for binary in metadata["rust-binaries"].values()
        if binary["kind"] == "lib" and binary["binary-name"] == library_name
    ]
    if len(candidates) != 1:
        raise ValueError(f"expected one {library_name} library harness in nextest metadata, found {len(candidates)}")
    binary = Path(candidates[0]["binary-path"])
    if not binary.is_absolute() or not binary.is_file():
        raise ValueError(f"archive-built library harness is missing or not absolute: {binary}")
    return binary


def archive_library_harness(archive_file: Path, library_name: str = "aft") -> Path:
    # The archive records producer-absolute paths. Read only its metadata, not
    # extracted executables: the original linked harness still has its local PDB.
    with tarfile.open(archive_file, mode="r|*") as archive:
        for member in archive:
            if member.name == "target/nextest/binaries-metadata.json":
                handle = archive.extractfile(member)
                if handle is None:
                    raise ValueError("nextest binaries metadata is not a file")
                return library_harness(json.load(handle), library_name)
    raise ValueError("nextest archive has no binaries metadata")


def kill_process_tree(process: subprocess.Popen) -> None:
    # The probes spawn their own helper processes. Killing only the direct child
    # would leave a hung grandchild holding the step open.
    if os.name == "nt":
        subprocess.run(
            ["taskkill", "/T", "/F", "/PID", str(process.pid)],
            capture_output=True, check=False,
        )
    else:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def run_bounded(command: list[str], cwd: Path, timeout_s: float) -> int:
    if os.name == "nt":
        process = subprocess.Popen(command, cwd=cwd, creationflags=subprocess.CREATE_NEW_PROCESS_GROUP)
    else:
        process = subprocess.Popen(command, cwd=cwd, start_new_session=True)
    try:
        return process.wait(timeout=timeout_s)
    except subprocess.TimeoutExpired:
        kill_process_tree(process)
        process.wait()
        print(f"serial Windows probe did not finish within {timeout_s:g}s and was killed", flush=True)
        return TIMEOUT_EXIT


def run_probe(binary: Path, probe: str, timeout_s: float = PROBE_TIMEOUT_S) -> int:
    name = PROBES[probe]
    # Cargo normally runs library tests from their package directory. Preserve
    # that working directory while using nextest's exact already-linked binary.
    cwd = ROOT / ("crates/aft-base" if probe == "backtrace" else "crates/aft")
    listing = subprocess.run(
        [str(binary), "--list", "--exact", name], cwd=cwd,
        capture_output=True, text=True, check=True, timeout=LIST_TIMEOUT_S,
    )
    if listing.stdout.splitlines().count(f"{name}: test") != 1:
        raise ValueError(f"serial probe is missing from the archive-built harness: {name}\n{listing.stdout}")
    print(f"Running serial Windows probe {name} from {binary}", flush=True)
    return run_bounded([str(binary), "--exact", name], cwd, timeout_s)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive-file", type=Path, required=True)
    parser.add_argument("--probe", choices=PROBES, required=True)
    args = parser.parse_args()
    print(f"Windows serial probe runner: Python {platform.python_version()}", flush=True)
    binary = archive_library_harness(args.archive_file, "aft_base" if args.probe == "backtrace" else "aft")
    return run_probe(binary, args.probe)


if __name__ == "__main__":
    raise SystemExit(main())
