#!/usr/bin/env bash
set -euo pipefail

# release.sh — Tag and push a new AFT release
#
# Usage:
#   ./scripts/release.sh 0.2.0        # release v0.2.0
#   ./scripts/release.sh 0.2.0 --dry  # preview without committing/pushing
#
# What it does:
#   1. Validates the version is semver
#   2. Checks for clean working tree (no uncommitted changes)
#   3. Syncs version across all 7 package files
#   4. Commits the version bump
#   5. Creates a git tag (v0.2.0)
#   6. Pushes commit + tag to origin
#   7. CI takes over: test → build → publish npm + GitHub release

VERSION="${1:-}"
DRY="${2:-}"

if [[ -z "$VERSION" ]]; then
  echo "Usage: ./scripts/release.sh <version> [--dry]"
  echo "  e.g. ./scripts/release.sh 0.2.0"
  exit 1
fi

if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.]+)?(\+[a-zA-Z0-9.]+)?$ ]]; then
  echo "Error: '$VERSION' is not valid semver (expected X.Y.Z)"
  exit 1
fi

TAG="v$VERSION"

# Check if tag already exists
if git rev-parse "$TAG" >/dev/null 2>&1; then
  echo "Error: tag '$TAG' already exists"
  exit 1
fi

# Check for clean working tree
if [[ -n "$(git status --porcelain)" ]]; then
  echo "Error: working tree is not clean — commit or stash changes first"
  git status --short
  exit 1
fi

# Check we're on main
BRANCH=$(git branch --show-current)
if [[ "$BRANCH" != "main" ]]; then
  echo "Warning: releasing from '$BRANCH' (not main)"
  read -rp "Continue? [y/N] " confirm
  if [[ "$confirm" != "y" && "$confirm" != "Y" ]]; then
    echo "Aborted."
    exit 1
  fi
fi

echo ""
echo "  Releasing AFT $TAG"
echo "  ─────────────────────"
echo ""

# Step 1: Sync versions
if [[ "$DRY" == "--dry" ]]; then
  echo "→ Version sync (dry run):"
  bun scripts/version-sync.mjs "$VERSION" --dry-run
  echo ""
  echo "[DRY RUN] Would commit, tag $TAG, and push to origin."
  exit 0
fi

echo "→ Running pre-release checks..."
echo ""

if [ "${SKIP_RUST_TESTS:-}" = "1" ]; then
  echo "  (skipping local cargo tests — SKIP_RUST_TESTS=1)"
  echo "  ↳ CI still runs the full suite on its own runners before publishing."
else
  echo "  cargo test..."
  cargo test --quiet 2>&1 || { echo "Error: Rust tests failed"; exit 1; }
fi

echo "  bun lint..."
bun run lint 2>&1 || { echo "Error: Lint failed"; exit 1; }

echo "  bun typecheck..."
bun run typecheck 2>&1 || { echo "Error: Typecheck failed"; exit 1; }

echo "  bun test..."
bun run test 2>&1 || { echo "Error: Plugin tests failed"; exit 1; }

if [ "${SKIP_DOCKER_E2E:-}" = "1" ]; then
  echo "  (skipping docker e2e — SKIP_DOCKER_E2E=1)"
elif command -v docker &>/dev/null && docker info &>/dev/null 2>&1; then
  echo "  docker e2e (Linux x64)..."
  # Build Linux x64 binary in Docker
  docker build --platform linux/amd64 -t aft-build-linux -f tests/docker/Dockerfile.build-linux . --quiet 2>&1 || { echo "Error: Docker Linux build failed"; exit 1; }
  # Extract binary to fixtures
  CID=$(docker create --platform linux/amd64 aft-build-linux true)
  docker cp "$CID:/build/target/release/aft" tests/docker/fixtures/aft-linux-x64
  docker rm "$CID" > /dev/null
  # Build E2E test image
  docker build --platform linux/amd64 -t aft-e2e-linux-x64 -f tests/docker/Dockerfile.linux-x64 . --quiet 2>&1 || { echo "Error: Docker E2E image build failed"; exit 1; }
  # Run E2E test
  docker run --rm --platform linux/amd64 aft-e2e-linux-x64 2>&1 || { echo "Error: Docker E2E tests failed"; exit 1; }
  # Clean up extracted binary (don't commit it)
  rm -f tests/docker/fixtures/aft-linux-x64
  echo "  ✓ Docker E2E passed"
else
  echo "  (skipping docker e2e — Docker not available)"
fi

echo "  ✓ All checks passed"
echo ""

echo "→ Syncing versions to $VERSION..."
bun scripts/version-sync.mjs "$VERSION"
echo ""

# Regenerate the JSON Schema asset so editor `$schema` URLs always resolve
# to a schema that matches the shipped config surface for this release.
echo "→ Rebuilding aft.schema.json..."
bun packages/opencode-plugin/scripts/build-schema.ts
echo ""

# Refresh bun.lock so workspace package versions match the bumped
# package.json files. Without this, CI's `bun install --frozen-lockfile`
# fails because the lockfile still pins the old versions.
echo "→ Refreshing bun.lock..."
bun install --silent 2>&1 || { echo "Error: bun install failed after version sync"; exit 1; }
echo ""

# Step 2: Commit (skip if versions were already at target)
echo "→ Committing version bump..."
git add -A
if git diff --cached --quiet; then
  echo "  (no changes — versions already at $VERSION)"
else
  git commit -m "release: $TAG"
fi

# Step 3: Tag
echo "→ Rebuilding local binary with new version..."
cargo build --release -p agent-file-tools --quiet 2>&1 || { echo "Error: Release build failed"; exit 1; }

# Update versioned cache only — never write to the flat cache path because
# other instances may be running a binary from there.
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/aft/bin"
mkdir -p "$CACHE_DIR/$TAG" && cp target/release/aft "$CACHE_DIR/$TAG/aft" 2>/dev/null && echo "  Updated $CACHE_DIR/$TAG/aft"

echo "→ Creating tag $TAG..."
git tag -a "$TAG" -m "Release $TAG"
echo ""

# Step 4: Push
echo "→ Pushing to origin..."
git push origin "$BRANCH"
git push origin "$TAG"
echo ""

echo "  ✓ Released $TAG"
echo "  → GitHub Actions will now: test → build → publish"
echo "  → Watch: https://github.com/cortexkit/aft/actions"
