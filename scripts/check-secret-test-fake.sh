#!/usr/bin/env bash
# Test-only vaults, hooks and shared fixtures must not be enabled by a release.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# Some macOS installations still put Python 3.9 first on PATH.
for python in python3 python3.14 python3.13 python3.12 python3.11; do
  if "$python" -c 'import tomllib' 2>/dev/null; then
    break
  fi
  python=
done
if [[ -z "$python" ]]; then
  printf 'secret-test-fake guard: Python 3.11+ with tomllib is required\n' >&2
  exit 1
fi

"$python" - <<'PY'
import json
import os
from pathlib import Path
import subprocess
import sys
import tomllib

PROTECTED = {"secret-test-fake", "test-hooks", "test-support", "test-scope-owner", "test-runner-notice-fixtures"}
STRICT = PROTECTED - {"test-support"}
ROOT = Path.cwd()
errors = []


def fail(manifest, message):
    errors.append(f"{manifest.relative_to(ROOT)}: {message}")


def manifests():
    for directory, children, files in os.walk(ROOT):
        if Path(directory) == ROOT:
            children[:] = [child for child in children if child not in {".git", "target", "node_modules"}]
        if "Cargo.toml" in files:
            yield Path(directory) / "Cargo.toml"


def inspect_features(manifest, features, siblings):
    # Any indirect activation from a default or ordinary feature must cross
    # into the protected set somewhere; reject the edge where it crosses.
    for source, entries in features.items():
        if source in PROTECTED and siblings is None:
            fail(manifest, f"non-workspace crate declares protected feature {source}")
        for entry in entries:
            dependency, slash, target = entry.partition("/")
            if (target if slash else entry) not in PROTECTED:
                if source in STRICT:
                    fail(manifest, f"{source} may only forward to a protected workspace feature: {entry}")
                continue
            if source == "default":
                fail(manifest, f"default enables a protected feature: {entry}")
            elif source not in PROTECTED:
                fail(manifest, f"ordinary feature {source} enables a protected feature: {entry}")
            elif slash != "/" or target not in (siblings or {}).get(dependency.removesuffix("?"), ()):
                fail(manifest, f"{source} must forward {entry} to a declared protected feature of a workspace sibling")


def inspect_dependency(manifest, name, features):
    for feature in sorted(PROTECTED.intersection(features)):
        fail(manifest, f"non-dev dependency {name} requests protected feature {feature}")


def inspect_other_manifest(manifest, document):
    inspect_features(manifest, document.get("features", {}), None)
    for table in (document, *document.get("target", {}).values()):
        for kind in ("dependencies", "build-dependencies", "build_dependencies"):
            for name, dependency in table.get(kind, {}).items():
                if isinstance(dependency, dict):
                    inspect_dependency(manifest, name, dependency.get("features", ()))


try:
    # No dependency resolution or network is needed. Cargo reports actual
    # members and expands renamed, target-specific and inherited dependencies.
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--offline", "--no-deps", "--format-version", "1"
    ]))
    member_ids = set(metadata["workspace_members"])
    members = {
        Path(package["manifest_path"]).resolve(): package
        for package in metadata["packages"] if package["id"] in member_ids
    }
    others = sorted({manifest for manifest in manifests() if manifest.resolve() not in members})
    for manifest in others:
        with manifest.open("rb") as source:
            inspect_other_manifest(manifest, tomllib.load(source))
    for manifest, member in sorted(members.items()):
        siblings = {}
        for dependency in member["dependencies"]:
            alias = dependency["rename"] or dependency["name"]
            if dependency["kind"] != "dev":
                inspect_dependency(manifest, alias, dependency["features"])
            targets = set()
            if local_path := dependency.get("path"):
                other_manifest = (Path(local_path) / "Cargo.toml").resolve()
                sibling = members.get(other_manifest)
                if sibling is not None and sibling["id"] != member["id"] and sibling["name"] == dependency["name"]:
                    targets.update(sibling["features"])
            # A forward must be safe for every target-specific spelling of the alias.
            siblings.setdefault(alias, targets).intersection_update(targets)
        inspect_features(manifest, member["features"], siblings)
except (OSError, ValueError, subprocess.CalledProcessError) as error:
    sys.exit(f"secret-test-fake guard: cannot inspect workspace manifests: {error}")

if errors:
    for error in errors:
        print(f"secret-test-fake guard: {error}", file=sys.stderr)
    sys.exit("secret-test-fake guard: FAILED")
print(f"secret-test-fake guard: ok ({len(members) + len(others)} manifests, {len(PROTECTED)} test-only features stay opt-in)")
fake_owners = sorted(member["name"] for member in members.values() if "secret-test-fake" in member["features"])
print(f"secret-test-fake guard: checked declarations in {', '.join(fake_owners) or '(none)'}")
PY
