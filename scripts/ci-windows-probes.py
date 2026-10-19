#!/usr/bin/env python3
"""Run serial Windows probes from the exact library harness built for an archive."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import platform
import subprocess
import tarfile


ROOT = Path(__file__).resolve().parents[1]
PROBES = {
    "tls": "semantic_index::tests::platform_verifier_tls_client_subprocess",
    "backtrace": "logging::tests::panic_hook_preserves_default_backtrace_behavior_and_persists",
}


def library_harness(metadata: dict) -> Path:
    candidates = [
        binary for binary in metadata["rust-binaries"].values()
        if binary["kind"] == "lib" and binary["binary-name"] == "aft"
    ]
    if len(candidates) != 1:
        raise ValueError(f"expected one aft library harness in nextest metadata, found {len(candidates)}")
    binary = Path(candidates[0]["binary-path"])
    if not binary.is_absolute() or not binary.is_file():
        raise ValueError(f"archive-built library harness is missing or not absolute: {binary}")
    return binary


def archive_library_harness(archive_file: Path) -> Path:
    # The archive records producer-absolute paths. Read only its metadata, not
    # extracted executables: the original linked harness still has its local PDB.
    with tarfile.open(archive_file, mode="r|*") as archive:
        for member in archive:
            if member.name == "target/nextest/binaries-metadata.json":
                handle = archive.extractfile(member)
                if handle is None:
                    raise ValueError("nextest binaries metadata is not a file")
                return library_harness(json.load(handle))
    raise ValueError("nextest archive has no binaries metadata")


def run_probe(binary: Path, probe: str) -> int:
    name = PROBES[probe]
    # Cargo normally runs library tests from their package directory. Preserve
    # that working directory while using nextest's exact already-linked binary.
    cwd = ROOT / "crates/aft"
    listing = subprocess.run(
        [str(binary), "--list", "--exact", name], cwd=cwd,
        capture_output=True, text=True, check=True,
    )
    if listing.stdout.splitlines().count(f"{name}: test") != 1:
        raise ValueError(f"serial probe is missing from the archive-built harness: {name}\n{listing.stdout}")
    print(f"Running serial Windows probe {name} from {binary}", flush=True)
    return subprocess.run([str(binary), "--exact", name], cwd=cwd, check=False).returncode


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive-file", type=Path, required=True)
    parser.add_argument("--probe", choices=PROBES, required=True)
    args = parser.parse_args()
    print(f"Windows serial probe runner: Python {platform.python_version()}", flush=True)
    binary = archive_library_harness(args.archive_file)
    return run_probe(binary, args.probe)


if __name__ == "__main__":
    raise SystemExit(main())
