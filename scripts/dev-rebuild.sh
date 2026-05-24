#!/usr/bin/env bash
# Rebuild the entire local AFT dev stack and stage it so a host restart
# (OpenCode, Pi, etc.) picks up the latest local changes without going
# through a release.
#
# What this does:
#   1. Reads the workspace version from crates/aft/Cargo.toml
#   2. Builds the Rust release binary
#   3. Builds the aft-bridge, opencode-plugin, and pi-plugin dists
#   4. Copies the release binary into the versioned cache that hosts
#      resolve via @cortexkit/aft-bridge (~/.cache/aft/bin/v<version>/aft)
#   5. Ad-hoc-signs the binary on macOS so Gatekeeper does not kill it
#   6. Prints a summary + restart hint
#
# Idempotent. Safe to run repeatedly. Pass --skip-rust to skip the cargo
# build when you only want to refresh plugin dists.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

SKIP_RUST=0
for arg in "$@"; do
  case "$arg" in
    --skip-rust) SKIP_RUST=1 ;;
    -h|--help)
      sed -n '1,/^set /p' "$0" | sed 's/^# \{0,1\}//' | head -n -1
      exit 0
      ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

# Read version from the canonical source (Cargo.toml).
VERSION="$(awk -F'"' '/^version = / {print $2; exit}' crates/aft/Cargo.toml)"
if [[ -z "$VERSION" ]]; then
  echo "error: could not read version from crates/aft/Cargo.toml" >&2
  exit 1
fi

CACHE_DIR="${HOME}/.cache/aft/bin/v${VERSION}"
BINARY_PATH="${CACHE_DIR}/aft"

echo "==> workspace version: v${VERSION}"
echo "==> cache target:      ${BINARY_PATH}"
echo

# 1. Rust release binary
if [[ "$SKIP_RUST" -eq 0 ]]; then
  echo "==> building Rust release binary"
  cargo build --release -p agent-file-tools
  echo
else
  echo "==> skipping Rust build (--skip-rust)"
  if [[ ! -f "target/release/aft" ]]; then
    echo "error: --skip-rust requires target/release/aft to exist already" >&2
    exit 1
  fi
fi

# 2. aft-bridge dist (must build first — plugins depend on it)
echo "==> building aft-bridge dist"
bun run --cwd packages/aft-bridge build
echo

# 3. opencode-plugin dist
echo "==> building OpenCode plugin dist"
bun run --cwd packages/opencode-plugin build
echo

# 4. pi-plugin dist
echo "==> building Pi plugin dist"
bun run --cwd packages/pi-plugin build
echo

# 5. Stage + sign the binary
echo "==> staging binary into versioned cache"
mkdir -p "$CACHE_DIR"
cp target/release/aft "$BINARY_PATH"
chmod +x "$BINARY_PATH"

if [[ "$(uname -s)" == "Darwin" ]]; then
  echo "==> ad-hoc signing for macOS"
  codesign --force --sign - "$BINARY_PATH"
fi
echo

# 6. Verify + summary
echo "==> verifying staged binary"
REPORTED_VERSION="$("$BINARY_PATH" --version 2>&1)"
echo "    ${REPORTED_VERSION}"

if ! echo "$REPORTED_VERSION" | grep -q "$VERSION"; then
  echo "warning: staged binary version mismatch (expected ${VERSION})" >&2
fi

echo
echo "==> done"
echo
echo "Plugin dists:"
echo "  packages/aft-bridge/dist"
echo "  packages/opencode-plugin/dist"
echo "  packages/pi-plugin/dist"
echo
echo "Binary:"
echo "  ${BINARY_PATH}"
echo
echo "Next steps:"
echo "  - Restart OpenCode so plugins pick up the new dist + binary"
echo "  - For Pi: restart the Pi agent or its containing host"
echo "  - If host already configured to load the local plugin path"
echo "    (file://... or absolute path), nothing else is required."
