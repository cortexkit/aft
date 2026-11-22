#!/usr/bin/env python3
"""Reject Cargo dependency paths that point up the AFT crate stack."""

import argparse
from collections import deque
import json
from pathlib import Path
import subprocess
import sys


FORBIDDEN_TARGETS = {
    "aft-engine": {"aft-tools", "aft-subc", "agent-file-tools"},
    "aft-tools": {"aft-subc", "agent-file-tools"},
    "aft-subc": {"agent-file-tools"},
}


def forbidden_paths(metadata):
    names = {package["id"]: package["name"] for package in metadata["packages"]}
    graph = {node["id"]: node["deps"] for node in metadata["resolve"]["nodes"]}
    for root in metadata["workspace_members"]:
        targets = FORBIDDEN_TARGETS.get(names[root])
        if not targets:
            continue

        pending: deque[tuple[str, ...]] = deque([(root,)])
        visited = {root}
        while pending:
            path = pending.popleft()
            package = path[-1]
            if names[package] in targets:
                yield tuple(names[key] for key in path)

            # Even a dev-only or platform-specific edge counts on an indirect path.
            for dep in graph[package]:
                child = dep["pkg"]
                if child not in visited:
                    visited.add(child)
                    pending.append((*path, child))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--metadata", type=Path, help="check a saved Cargo metadata document")
    args = parser.parse_args()
    if args.metadata is None:
        metadata = json.loads(subprocess.check_output([
            "cargo", "metadata", "--locked", "--all-features", "--format-version", "1",
        ]))
    else:
        metadata = json.loads(args.metadata.read_bytes())
    violations = list(forbidden_paths(metadata))
    if violations:
        for path in violations:
            print(
                f"forbidden dependency: {path[0]} -> {path[-1]} (path: {' -> '.join(path)})",
                file=sys.stderr,
            )
        return 1
    print("No upward dependency paths among AFT crates")
    return 0


if __name__ == "__main__":
    sys.exit(main())
