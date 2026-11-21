#!/usr/bin/env python3
"""Run the source guard and its registration fixtures with the Rust parser."""

import argparse
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive-file", type=Path, help="reuse a nextest archive")
    args = parser.parse_args()
    command = ["cargo", "nextest", "run", "--no-tests", "fail"]
    if args.archive_file:
        command += ["--archive-file", str(args.archive_file.resolve())]
    else:
        command += ["-p", "agent-file-tools", "--lib", "--features", "wasm-grammars,test-hooks"]
    command += [
        "-E", "package(=agent-file-tools) & kind(lib) & test(~engine_source_guard_tests::)",
    ]
    return subprocess.call(command, cwd=Path(__file__).resolve().parents[1])


if __name__ == "__main__":
    raise SystemExit(main())
