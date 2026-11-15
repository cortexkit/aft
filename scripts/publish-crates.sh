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

# Allow the leaf versions to propagate to crates.io's sparse index before the
# main crate's verification resolves them.
sleep 30
cargo publish --package agent-file-tools \
  || { ec=$?; cargo publish --package agent-file-tools --dry-run 2>&1 | grep -q "already exists" && true || exit $ec; }
