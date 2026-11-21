#!/usr/bin/env python3
"""Check the aft-engine compiler inputs from a native Cargo JSON build."""

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys


ENGINE = Path(__file__).resolve().parents[1]
WORKSPACE = ENGINE.parents[1]
UPPER_SOURCES = WORKSPACE / "crates/aft/src"


def make_paths(text):
    """Read the dependency list of the first Make rule (not later phony rules)."""
    logical = text.replace("\\\n", " ").splitlines()[0]
    # A Windows drive colon is followed by a slash, not a Make separator.
    separator = re.search(r"(?<!\\):(?=\s|$)", logical)
    if separator is None:
        raise ValueError("no Make rule separator in dep-info")
    word = []
    tokens = iter(logical[separator.end():])
    for ch in tokens:
        if ch == "\\":
            escaped = next(tokens, "")
            word.append(escaped if (escaped.isspace() or escaped in ("\\", "#", ":")) else ch + escaped)
        elif ch.isspace():
            if word:
                yield "".join(word)
                word.clear()
        else:
            word.append(ch)
    if word:
        yield "".join(word)


def dep_file(artifact):
    candidates = set()
    for filename in artifact["filenames"]:
        output = Path(filename)
        stem = output.stem.removeprefix("lib")
        # Cargo also copies a stable, unhashed library name into target/debug.
        if stem.startswith("aft_engine-"):
            candidates.add(output.with_name(stem + ".d"))
    if len(candidates) != 1:
        raise ValueError(f"expected one hashed aft-engine unit, got {sorted(candidates)}")
    path, = candidates
    if not path.is_file():
        raise ValueError(f"missing aft-engine dep-info: {path}")
    return path


def engine_units(messages, required_features):
    units = {False: set(), True: set()}
    success = False
    for line in messages:
        message = json.loads(line)
        if message["reason"] == "build-finished":
            success = message["success"]
        if message["reason"] != "compiler-artifact":
            continue
        if Path(message["manifest_path"]).resolve() != ENGINE / "Cargo.toml":
            continue
        if message["target"]["kind"] == ["lib"] and message["target"]["name"] == "aft_engine":
            if not required_features.issubset(message["features"]):
                continue
            units[message["profile"]["test"]].add(dep_file(message))
    if not success:
        raise ValueError("Cargo JSON does not contain a successful build-finished record")
    return units


def audit_sources(dep_files, packaged=None):
    violations = []
    for dep in sorted(dep_files):
        inputs = set(make_paths(dep.read_text(encoding="utf-8")))
        if not inputs:
            raise ValueError(f"empty input list in {dep}")
        for spelling in sorted(inputs):
            path = (WORKSPACE / spelling).resolve()
            if path.is_relative_to(UPPER_SOURCES):
                violations.append(f"{dep.name}: upper source {path}")
            if packaged is not None and path.is_relative_to(ENGINE):
                relative = path.relative_to(ENGINE).as_posix()
                if relative not in packaged:
                    violations.append(f"{dep.name}: not packaged: {relative}")
        print(f"{dep.name}: audited {len(inputs)} source inputs")
    return violations


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("messages", type=Path, help="stdout from Cargo --message-format=json")
    parser.add_argument(
        "--packaged", action="store_true",
        help="check the library built with test-support,test-hooks against its package file list",
    )
    args = parser.parse_args()
    required = {"test-hooks", "test-support" if args.packaged else "wasm-grammars"}
    try:
        with args.messages.open(encoding="utf-8") as stream:
            units = engine_units(stream, required)
        if not units[False] or (not args.packaged and not units[True]):
            raise ValueError(f"missing aft-engine lib or lib-test with {', '.join(sorted(required))}")
        if args.packaged:
            packaged = set(subprocess.check_output([
                "cargo", "package", "--locked", "-p", "aft-engine", "--list",
            ], cwd=WORKSPACE, text=True).splitlines())
            violations = audit_sources(units[False], packaged)
        else:
            violations = audit_sources(units[False] | units[True])
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"source-input check: {error}", file=sys.stderr)
        return 1
    if violations:
        print("\n".join(violations), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
