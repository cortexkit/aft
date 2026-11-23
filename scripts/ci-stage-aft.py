#!/usr/bin/env python3
"""Build aft and stage the executable (and optional MSVC PDB) for this CI job."""

from __future__ import annotations

import argparse
from collections.abc import Iterable
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


def read_aft_artifact(lines: Iterable[str]) -> dict | None:
    artifact = None
    for line in lines:
        # Cargo may relay plain stdout from tools in addition to its JSON records.
        if not line.startswith("{"):
            print(line, end="", flush=True)
            continue
        message = json.loads(line)
        if message["reason"] == "compiler-message":
            print(message["message"]["rendered"] or "", end="", file=sys.stderr, flush=True)
        if (message["reason"] == "compiler-artifact" and message["target"]["name"] == "aft"
                and "bin" in message["target"]["kind"] and message.get("executable")):
            artifact = message
    return artifact


def stage_artifact(artifact: dict | None, target_dir: Path, job_dir: Path, with_pdb: bool) -> None:
    if artifact is None:
        raise ValueError("Cargo did not report an executable for the aft binary")
    executable = Path(artifact["executable"]).resolve(strict=True)
    if not executable.is_relative_to(target_dir) or executable.name not in ("aft", "aft.exe"):
        raise ValueError(f"Cargo reported an unexpected aft executable: {executable}")
    outputs = [executable]
    if with_pdb:
        pdb = executable.with_suffix(".pdb")
        if executable.suffix != ".exe" or pdb not in (Path(p).resolve() for p in artifact["filenames"]):
            raise ValueError("Cargo did not report the aft.pdb paired with the aft executable")
        outputs.append(pdb)
    for source in outputs:
        # Copy rather than hardlink: ad-hoc signing must not alter Cargo's cache.
        destination = job_dir / source.name
        shutil.copy2(source, destination)
        print(f"Staged {destination} from {source}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pdb", action="store_true", help="also stage the PDB from the same Cargo artifact")
    parser.add_argument("command", nargs=argparse.REMAINDER, help="cargo build command and arguments")
    args = parser.parse_args()
    if args.command[:2] != ["cargo", "build"]:
        parser.error("expected a cargo build command")
    job_dir = Path(os.environ["AFT_CI_JOB_DIR"]).resolve(strict=True)
    target_dir = Path(os.environ["CARGO_TARGET_DIR"]).resolve()
    # Cargo reports the final target-dir path, even for reused artifacts.
    # Two builds can overwrite that path unless each job has its own target dir.
    if target_dir == job_dir or not target_dir.is_relative_to(job_dir):
        parser.error("CARGO_TARGET_DIR must be inside AFT_CI_JOB_DIR")
    with subprocess.Popen([*args.command, "--message-format=json-render-diagnostics"],
                          stdout=subprocess.PIPE, text=True, encoding="utf-8") as cargo:
        assert cargo.stdout is not None
        artifact = read_aft_artifact(cargo.stdout)
        if code := cargo.wait():
            raise SystemExit(code)
    try:
        stage_artifact(artifact, target_dir, job_dir, args.pdb)
    except (OSError, ValueError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
