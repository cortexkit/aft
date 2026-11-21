#!/usr/bin/env python3
"""Reject any Cargo dependency path from aft-engine to agent-file-tools."""

import argparse
from collections import deque
import json
from pathlib import Path
import subprocess
import sys


def upper_dependency(metadata):
    names = {package["id"]: package["name"] for package in metadata["packages"]}
    engine, = (key for key in metadata["workspace_members"] if names[key] == "aft-engine")
    graph = {node["id"]: node["deps"] for node in metadata["resolve"]["nodes"]}
    pending = deque([(engine,)])
    visited = {engine}
    while pending:
        path = pending.popleft()
        package = path[-1]
        if names[package] == "agent-file-tools":
            return path
        for dep in graph[package]:
            child = dep["pkg"]
            if child in visited:
                continue
            allowed = {None, "normal", "build"}
            if package == engine:
                allowed.add("dev")
            if not any(kind["kind"] in allowed for kind in dep["dep_kinds"]):
                continue
            visited.add(child)
            pending.append((*path, child))
    return None


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
    forbidden = upper_dependency(metadata)
    if forbidden is not None:
        print("engine depends on upper package: " + " -> ".join(forbidden), file=sys.stderr)
        return 1
    print("aft-engine has no dependency path to agent-file-tools")
    return 0


if __name__ == "__main__":
    sys.exit(main())
