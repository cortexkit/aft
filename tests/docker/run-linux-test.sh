#!/usr/bin/env bash
# Build and run Linux E2E tests in Docker.
# Uses aimock + OpenCode to test the full AFT plugin stack.
#
# Usage:
#   ./tests/docker/run-linux-test.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

echo "Building Linux x64 E2E test image..."
echo "(Installs OpenCode + AFT plugin + aimock from npm)"
echo ""

docker build \
    --platform linux/amd64 \
    -f "$SCRIPT_DIR/Dockerfile.linux-x64" \
    -t aft-e2e-linux-x64 \
    "$REPO_ROOT"

echo ""
echo "Running E2E tests..."
docker run --rm --platform linux/amd64 aft-e2e-linux-x64
