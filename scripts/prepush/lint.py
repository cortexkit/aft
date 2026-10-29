#!/usr/bin/env python3
"""Run the scoped Biome package at the exact version resolved by bun.lock."""
import re
import subprocess
import sys
from pathlib import Path


def locked_version(path=Path("bun.lock")):
    matches = re.findall(r'"@biomejs/biome"\s*:\s*\[\s*"@biomejs/biome@([^"\s]+)"', path.read_text())
    if len(matches) != 1 or not re.fullmatch(r"\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?", matches[0]):
        raise ValueError("Expected one exact @biomejs/biome version in bun.lock")
    return matches[0]


def lint():
    expected = locked_version()
    package = f"@biomejs/biome@{expected}"
    print(f"Using locked {package}", flush=True)
    version = subprocess.run(["bunx", package, "--version"], text=True, capture_output=True)
    print(version.stdout, end="", flush=True)
    if version.stderr:
        print(version.stderr, end="", file=sys.stderr)
    if version.returncode:
        return version.returncode
    reported = re.fullmatch(r"Version:\s*(\S+)\s*", version.stdout.strip())
    if reported is None or reported.group(1) != expected:
        print(f"Expected Biome Version: {expected}; got {version.stdout.strip()!r}; refusing lint", file=sys.stderr)
        return 1
    checked = subprocess.run(["bunx", package, "check", "."], text=True, capture_output=True)
    print(checked.stdout, end="", flush=True)
    if checked.stderr:
        print(checked.stderr, end="", file=sys.stderr)
    if checked.returncode:
        return checked.returncode
    if not re.search(r"\bChecked [1-9]\d* files?\b", checked.stdout):
        print("Pinned Biome did not report checking any files; refusing an empty success", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(lint())
