#!/usr/bin/env python3
"""Refuse checkout conversion of the byte-exact fixture families."""
import fnmatch
import subprocess
from pathlib import PurePosixPath


PATTERNS = (
    "crates/aft/tests/fixtures/gh_shim/*",
    "crates/aft/tests/fixtures/pty_render/*.raw",
    "crates/aft/tests/fixtures/pty_render/*.xterm.txt",
    "crates/aft/tests/fixtures/callgraph/truncation/*.txt",
    "crates/aft/tests/fixtures/outline_summaries/*",
    "crates/aft/tests/fixtures/bash/*",
    "crates/aft/tests/fixtures/inspect/noise-renderer.json",
    "benchmarks/aft-search/*.json",
    "benchmarks/aft-search/*.bin",
    "benchmarks/aft-search/manifest.sha256",
    "crates/aft/src/exec_remote/fixtures/reports/*",
    "crates/aft/tests/fixtures/rich_ui/*",
    "crates/aft/tests/fixtures/agent_data/*",
    "crates/aft/tests/fixtures/secret/*",
)


def main():
    tracked = subprocess.check_output(["git", "ls-files", "-z"]).split(b"\0")
    paths = [p for p in tracked if p and any(
        fnmatch.fnmatchcase(p.decode(), pattern)
        and (not pattern.startswith("benchmarks/") or PurePosixPath(p.decode()).parent == PurePosixPath("benchmarks/aft-search"))
        for pattern in PATTERNS
    )]
    if not paths:
        raise SystemExit("No byte-exact fixtures found; attribute check did not run")
    attrs = subprocess.check_output(
        ["git", "check-attr", "-z", "--stdin", "text", "eol"],
        input=b"\0".join(paths) + b"\0",
    ).split(b"\0")
    by_path = {}
    for index in range(0, len(attrs) - 1, 3):
        path, key, value = attrs[index:index + 3]
        by_path.setdefault(path, {})[key] = value
    bad = [p.decode() for p in paths if not (
        by_path[p].get(b"text") == b"unset" or by_path[p].get(b"eol") == b"lf"
    )]
    if bad:
        raise SystemExit("Missing LF/-text pins:\n" + "\n".join(bad))
    print(f"Checked checkout-conversion pins for {len(paths)} byte-exact fixtures")


if __name__ == "__main__":
    main()
