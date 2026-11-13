#!/usr/bin/env python3
"""Build/list the integration archive without compiling the separate unit buckets."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess


FILTER = "kind(test) - binary(=watcher_integration)"
ROOT = Path(__file__).resolve().parents[1]


def integration_targets(metadata: dict) -> list[str]:
    members = set(metadata["workspace_members"])
    names = sorted({
        target["name"]
        for package in metadata["packages"] if package["id"] in members
        for target in package["targets"]
        if target["kind"] == ["test"] and target.get("test", True)
        and target["name"] != "watcher_integration"
    })
    if not names:
        raise ValueError("no enabled non-watcher integration test targets in the workspace")
    return names


def command(metadata: dict, mode: str, archive_file: str | None = None) -> list[str]:
    # A nextest filterset selects archive/list contents only: cargo still builds
    # every default test target unless explicit Cargo target selectors are used.
    args = ["cargo", "nextest", mode, "--workspace", "--features", "wasm-grammars,test-hooks"]
    for name in integration_targets(metadata):
        args.extend(["--test", name])
    args.extend(["-E", FILTER])
    if mode == "archive":
        if not archive_file:
            raise ValueError("archive mode requires --archive-file")
        args.extend(["--archive-file", archive_file])
    elif mode == "list":
        args.extend(["--profile", "gate", "-T", "json", "--cargo-quiet"])
    else:
        raise ValueError(f"unsupported archive operation: {mode}")
    return args


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("archive", "list"))
    parser.add_argument("--archive-file")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=ROOT,
    ))
    selected = integration_targets(metadata)
    print(f"Integration Cargo target split: {len(selected)} targets: {', '.join(selected)}", flush=True)
    argv = command(metadata, args.mode, args.archive_file)
    if args.mode == "list":
        if not args.output:
            parser.error("list mode requires --output")
        args.output.parent.mkdir(parents=True, exist_ok=True)
        with args.output.open("w") as output:
            subprocess.run(argv, cwd=ROOT, stdout=output, check=True)
    else:
        subprocess.run(argv, cwd=ROOT, check=True)


if __name__ == "__main__":
    main()
