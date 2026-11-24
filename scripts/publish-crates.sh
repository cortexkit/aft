#!/usr/bin/env bash
# Run from the release source checkout, which may predate this helper on retries.
# Match the release job's default bash -e shell: the already-exists fallback
# tests dry-run output even when cargo exits nonzero, using grep's exit status.
set -eu

TAG="${1:?Usage: publish-crates.sh <vX.Y.Z>}"
if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[[:alnum:].-]+)?(\+[[:alnum:].-]+)?$ ]]; then
  echo "::error::Expected a release tag (vX.Y.Z), got $TAG" >&2
  exit 1
fi

# Older tags lack the split crates. Check the tagged checkout's tracked manifests
# before version-sync or any upload so a broken split cannot publish partially.
python3 - "$TAG" <<'PY'
from pathlib import Path
import subprocess
import sys
import tomllib

missing = {}
for crate in ("aft-edit", "aft-engine", "aft-tools", "aft-subc"):
    manifest = Path("crates") / crate / "Cargo.toml"
    if not manifest.is_file():
        missing[crate] = manifest
if not missing:
    sys.exit(0)

paths = subprocess.check_output(
    ["git", "ls-files", "-z", "--", "Cargo.toml", ":(glob)**/Cargo.toml"], text=True
)
for path in filter(None, paths.split("\0")):
    manifest_path = Path(path)
    with manifest_path.open("rb") as source:
        manifest = tomllib.load(source)
    workspace = manifest.get("workspace", {})
    names = {manifest.get("package", {}).get("name")}
    local_paths = {
        (manifest_path.parent / member).resolve()
        for key in ("members", "default-members")
        for member in workspace.get(key, [])
    }
    for table in (manifest, workspace, *manifest.get("target", {}).values()):
        for kind in ("dependencies", "dev-dependencies", "build-dependencies"):
            for name, spec in table.get(kind, {}).items():
                if isinstance(spec, dict):
                    name = spec.get("package", name)
                    if "path" in spec:
                        local_paths.add((manifest_path.parent / spec["path"]).resolve())
                names.add(name)

    for crate, missing_path in missing.items():
        if crate in names or missing_path.parent.resolve() in local_paths:
            sys.exit(f"::error::{sys.argv[1]} requires {crate} but is missing {missing_path} (declared in {path})")
PY

# Cargo must publish the requested tag's version, not a stale manifest version.
GITHUB_REF_NAME="$TAG" node scripts/version-sync.mjs --from-tag
TAG_VERSION="${TAG#v}"
CARGO_VERSION=$(grep '^version' crates/aft/Cargo.toml | head -1 | sed -E 's/version = "([^"]+)"/\1/')
if [ "$TAG_VERSION" != "$CARGO_VERSION" ]; then
  echo "::error::Tag $TAG wants $TAG_VERSION but Cargo.toml has $CARGO_VERSION after version-sync. version-sync.mjs broken?" >&2
  exit 1
fi

# Publish path dependencies first so the main crate can resolve them on crates.io.
# Keep `true`, not `exit 0`, in each already-published fallback: an early exit
# would silently skip the remaining crates after a partial publication.
cargo publish --package aft-tokenizer \
  || { ec=$?; cargo publish --package aft-tokenizer --dry-run 2>&1 | grep -q "already exists" && true || exit $ec; }
cargo publish --package aft-secret-rules \
  || { ec=$?; cargo publish --package aft-secret-rules --dry-run 2>&1 | grep -q "already exists" && true || exit $ec; }
cargo publish --package aft-store \
  || { ec=$?; cargo publish --package aft-store --dry-run 2>&1 | grep -q "already exists" && true || exit $ec; }
sleep 30
cargo publish --package aft-base \
  || { ec=$?; cargo publish --package aft-base --dry-run 2>&1 | grep -q "already exists" && true || exit $ec; }
sleep 30
cargo publish --package aft-db \
  || { ec=$?; cargo publish --package aft-db --dry-run 2>&1 | grep -q "already exists" && true || exit $ec; }
sleep 30
cargo publish --package aft-parse \
  || { ec=$?; cargo publish --package aft-parse --dry-run 2>&1 | grep -q "already exists" && true || exit $ec; }
if [[ -f crates/aft-edit/Cargo.toml ]]; then
  sleep 30
  cargo publish --package aft-edit \
    || { ec=$?; cargo publish --package aft-edit --dry-run 2>&1 | grep -q "already exists" && true || exit $ec; }
fi
if [[ -f crates/aft-engine/Cargo.toml ]]; then
  sleep 30
  cargo publish --package aft-engine \
    || { ec=$?; cargo publish --package aft-engine --dry-run 2>&1 | grep -q "already exists" && true || exit $ec; }
fi
if [[ -f crates/aft-tools/Cargo.toml ]]; then
  sleep 30
  cargo publish --package aft-tools \
    || { ec=$?; cargo publish --package aft-tools --dry-run 2>&1 | grep -q "already exists" && true || exit $ec; }
fi
if [[ -f crates/aft-subc/Cargo.toml ]]; then
  sleep 30
  cargo publish --package aft-subc \
    || { ec=$?; cargo publish --package aft-subc --dry-run 2>&1 | grep -q "already exists" && true || exit $ec; }
fi

# Allow the dependency versions to propagate to crates.io's sparse index before the
# main crate's verification resolves them.
sleep 30
cargo publish --package agent-file-tools \
  || { ec=$?; cargo publish --package agent-file-tools --dry-run 2>&1 | grep -q "already exists" && true || exit $ec; }
