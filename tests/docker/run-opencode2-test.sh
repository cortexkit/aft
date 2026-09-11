#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HARNESS_DIR="$SCRIPT_DIR/opencode2/harness"
IMAGE="${AFT_OPENCODE2_IMAGE:-aft-e2e-opencode2-linux}"
ARTIFACT_ROOT="${AFT_E2E_ARTIFACT_ROOT:-$REPO_ROOT/.tmp/opencode2-e2e}"
RUN_ID="${AFT_E2E_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-${GITHUB_RUN_ID:-local}-$$}"

command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 2; }
command -v bun >/dev/null 2>&1 || { echo "bun is required to read the pinned host version" >&2; exit 2; }

HOST_VERSION="$(bun "$HARNESS_DIR/driver.ts" --print-host-version)"
V1_HOST_VERSION="$(tr -d '[:space:]' < "$REPO_ROOT/.github/opencode-version.txt")"
if [[ -z "$V1_HOST_VERSION" ]]; then
  echo ".github/opencode-version.txt is empty" >&2
  exit 2
fi
GIT_SHA="$(git -C "$REPO_ROOT" rev-parse HEAD)"
if [[ ! "$GIT_SHA" =~ ^[0-9a-f]{40}$ ]]; then
  echo "could not resolve a full checkout SHA" >&2
  exit 2
fi
mkdir -p "$ARTIFACT_ROOT"

printf 'Building OpenCode 2 harness image (%s, checkout %s)...\n' "$HOST_VERSION" "$GIT_SHA"
docker build \
  --platform linux/amd64 \
  --build-arg "AFT_GIT_SHA=$GIT_SHA" \
  --build-arg "OPENCODE2_VERSION=$HOST_VERSION" \
  --build-arg "OPENCODE1_VERSION=$V1_HOST_VERSION" \
  --file "$HARNESS_DIR/Dockerfile" \
  --tag "$IMAGE" \
  "$REPO_ROOT"

run_args=(
  --rm
  --platform linux/amd64
  --volume "$ARTIFACT_ROOT:/artifacts"
  --env "AFT_CHECKOUT_SHA=$GIT_SHA"
  --env "AFT_E2E_RUN_ID=$RUN_ID"
  --env "AFT_E2E_RUN_ROOT=/artifacts/$RUN_ID"
  --env "AFT_E2E_CONCURRENCY=${AFT_E2E_CONCURRENCY:-4}"
)
if [[ -n "${AFT_E2E_SCENARIO:-}" ]]; then
  run_args+=(--env "AFT_E2E_SCENARIO=$AFT_E2E_SCENARIO")
fi
if [[ -n "${AFT_OPENCODE2_SCHEMA_OBSERVATION:-}" ]]; then
  run_args+=(--env "AFT_OPENCODE2_SCHEMA_OBSERVATION=/artifacts/$RUN_ID/contract-observations/host-schema-rejection.json")
fi
if [[ -n "${AFT_BINARY_PATH:-}" ]]; then
  binary_dir="$(cd "$(dirname "$AFT_BINARY_PATH")" && pwd)"
  binary_name="$(basename "$AFT_BINARY_PATH")"
  if [[ ! -f "$binary_dir/build-info.json" ]]; then
    echo "same-SHA artifact requires build-info.json beside AFT_BINARY_PATH" >&2
    exit 2
  fi
  run_args+=(--volume "$binary_dir:/aft-artifact:ro" --env "AFT_BINARY_PATH=/aft-artifact/$binary_name")
fi

printf 'Running OpenCode 2 matrix; forensics: %s/%s\n' "$ARTIFACT_ROOT" "$RUN_ID"
docker run "${run_args[@]}" "$IMAGE" "$@"
