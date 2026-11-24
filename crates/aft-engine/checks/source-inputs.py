#!/usr/bin/env python3
"""Check split-crate compiler inputs from a Cargo JSON build."""

import argparse
from dataclasses import dataclass
import json
from pathlib import Path
import re
import subprocess
import sys


CRATES = Path(__file__).resolve().parents[2]
WORKSPACE = CRATES.parent
ROOT_IDENTITY_FIXTURE = Path("tests/fixtures/root_identity_upgrade/cases.json")


@dataclass(frozen=True)
class Audit:
    name: str
    upper_crates: tuple[str, ...]
    native_features: tuple[str, ...]
    packaged_features: tuple[str, ...]

    @property
    def root(self):
        return CRATES / self.name


# Match the feature selections in .github/workflows/_unit-suite.yml. The
# workspace test build forwards test-support to the lower crates via test-hooks.
ABOVE_EDIT = ("aft", "aft-engine", "aft-tools", "aft-subc")
ABOVE_LOWER = (*ABOVE_EDIT, "aft-edit")
AUDITS = (
    Audit("aft-base", ABOVE_LOWER,
          ("test-hooks", "test-support"), ("test-hooks", "test-support")),
    Audit("aft-db", ABOVE_LOWER,
          ("test-hooks", "test-support"), ("test-hooks", "test-support")),
    Audit("aft-edit", ABOVE_EDIT,
          ("test-hooks", "test-support"), ("test-hooks", "test-support")),
    Audit("aft-engine", ("aft", "aft-tools", "aft-subc"),
          ("test-hooks", "wasm-grammars"), ("test-hooks", "test-support")),
    Audit("aft-tools", ("aft", "aft-subc"),
          ("test-hooks", "wasm-grammars"), ("test-hooks", "test-support")),
    Audit("aft-subc", ("aft",),
          ("test-hooks",), ("test-hooks", "test-runner-notice-fixtures")),
)


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


def dep_file(artifact, audit):
    candidates = set()
    prefix = artifact["target"]["name"] + "-"
    for filename in artifact["filenames"]:
        output = Path(filename)
        stem = output.stem.removeprefix("lib")
        # Cargo also copies a stable, unhashed library name into target/debug.
        if stem.startswith(prefix):
            candidates.add(output.with_name(stem + ".d"))
    if len(candidates) != 1:
        raise ValueError(f"expected one hashed {audit.name} unit, got {sorted(candidates)}")
    path, = candidates
    if not path.is_file():
        raise ValueError(f"missing {audit.name} dep-info: {path}")
    return path


def cargo_units(messages, packaged):
    manifests = {audit.root / "Cargo.toml": audit for audit in AUDITS}
    required = {
        audit: set(audit.packaged_features if packaged else audit.native_features)
        for audit in AUDITS
    }
    profiles = (False,) if packaged else (False, True)
    units = {(audit, test): set() for audit in AUDITS for test in profiles}
    success = False
    for line in messages:
        message = json.loads(line)
        if message["reason"] == "build-finished":
            success = message["success"]
        if message["reason"] != "compiler-artifact":
            continue
        audit = manifests.get(Path(message["manifest_path"]).resolve())
        if audit is None:
            continue
        target = message["target"]
        test = message["profile"]["test"]
        if (test in profiles and target["kind"] == ["lib"]
                and target["name"] == audit.name.replace("-", "_")
                and required[audit].issubset(message["features"])):
            units[audit, test].add(dep_file(message, audit))
    if not success:
        raise ValueError("Cargo JSON does not contain a successful build-finished record")
    for (audit, test), dep_files in units.items():
        if not dep_files:
            kind = "lib-test" if test else "lib"
            raise ValueError(f"missing {audit.name} {kind} with {', '.join(sorted(required[audit]))}")
    return units


def audit_sources(audit, dep_files, packaged=None):
    upper_roots = [CRATES / name / "src" for name in audit.upper_crates]
    violations = []
    for dep in sorted(dep_files):
        inputs = set(make_paths(dep.read_text(encoding="utf-8")))
        if not inputs:
            raise ValueError(f"empty input list in {dep}")
        for spelling in sorted(inputs):
            path = (WORKSPACE / spelling).resolve()
            if any(path.is_relative_to(root) for root in upper_roots):
                violations.append(f"{dep.name}: upper source {path}")
            if packaged is not None and path.is_relative_to(audit.root):
                relative = path.relative_to(audit.root).as_posix()
                if relative not in packaged:
                    violations.append(f"{dep.name}: not packaged: {relative}")
        print(f"{dep.name}: audited {len(inputs)} source inputs")
    return violations


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("messages", type=Path, help="stdout from Cargo --message-format=json")
    parser.add_argument(
        "--packaged", action="store_true",
        help="check each library built with its opt-in test features against its package file list",
    )
    args = parser.parse_args()
    violations = []
    try:
        upper_fixture = CRATES / "aft" / ROOT_IDENTITY_FIXTURE
        lower_fixture = CRATES / "aft-db" / ROOT_IDENTITY_FIXTURE
        if upper_fixture.exists() and upper_fixture.read_bytes() != lower_fixture.read_bytes():
            violations.append("aft and aft-db root-identity fixtures differ")
        with args.messages.open(encoding="utf-8") as stream:
            units = cargo_units(stream, args.packaged)
        for audit in AUDITS:
            dep_files = units[audit, False]
            packaged = None
            if args.packaged:
                packaged = set(subprocess.check_output([
                    "cargo", "package", "--locked", "-p", audit.name, "--list",
                    "--features", ",".join(audit.packaged_features),
                ], cwd=WORKSPACE, text=True).splitlines())
            else:
                dep_files = dep_files | units[audit, True]
            violations.extend(audit_sources(audit, dep_files, packaged))
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"source-input check: {error}", file=sys.stderr)
        return 1
    if violations:
        print("\n".join(violations), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
