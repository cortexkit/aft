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
if [[ ! -w "$ARTIFACT_ROOT" ]]; then
  echo "$ARTIFACT_ROOT is not writable by $(id -un); an earlier root-owned run may still own it" >&2
  exit 2
fi

printf 'Building OpenCode 2 harness image (%s, checkout %s)...\n' "$HOST_VERSION" "$GIT_SHA"

# A same-SHA artifact makes the image's own release build redundant: the
# harness exercises the mounted binary, so compiling a second copy inside the
# image only spends wall clock. Stage it into the build context and select the
# prebuilt stage; without an artifact the image still builds from the checkout.
build_args=(
  --build-arg "AFT_GIT_SHA=$GIT_SHA"
  --build-arg "OPENCODE2_VERSION=$HOST_VERSION"
  --build-arg "OPENCODE1_VERSION=$V1_HOST_VERSION"
)
staged_artifact="$REPO_ROOT/.aft-opencode2-artifact"
stage_cleanup() { rm -rf "$staged_artifact"; }
if [[ -n "${AFT_BINARY_PATH:-}" ]]; then
  source_dir="$(cd "$(dirname "$AFT_BINARY_PATH")" && pwd)"
  if [[ ! -f "$source_dir/build-info.json" ]]; then
    echo "same-SHA artifact requires build-info.json beside AFT_BINARY_PATH" >&2
    exit 2
  fi
  trap stage_cleanup EXIT
  stage_cleanup
  mkdir -p "$staged_artifact"
  cp "$source_dir/aft" "$source_dir/aft.real" "$source_dir/build-info.json" "$staged_artifact/"
  build_args+=(--build-arg "AFT_BINARY_SOURCE=prebuilt")
  printf 'Using the same-SHA artifact from %s instead of building in the image\n' "$source_dir"
fi

docker build \
  --platform linux/amd64 \
  "${build_args[@]}" \
  --file "$HARNESS_DIR/Dockerfile" \
  --tag "$IMAGE" \
  "$REPO_ROOT"
stage_cleanup

# Run as the invoking uid/gid, not root. The scenario roots are mkdtemp'd (mode
# 0700) under the bind-mounted artifact root, so a root-owned run leaves a
# forensics tree its own caller cannot traverse: CI's collection step fails with
# `EACCES: permission denied, scandir` and uploads an empty bundle, and a local
# run leaves directories the developer has to sudo into. A post-hoc `chown -R`
# would need privileges the collector does not have; owning the files correctly
# in the first place works the same way in CI and on a laptop.
CONTAINER_USER="${AFT_E2E_CONTAINER_USER:-$(id -u):$(id -g)}"

run_args=(
  --rm
  --name "aft-opencode2-$RUN_ID"
  --platform linux/amd64
  --user "$CONTAINER_USER"
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
  run_args+=(--volume "$binary_dir:/aft-artifact:ro" --env "AFT_BINARY_PATH=/aft-artifact/$binary_name")
fi

printf 'Running OpenCode 2 matrix; forensics: %s/%s\n' "$ARTIFACT_ROOT" "$RUN_ID"
# `docker run` in the foreground does not forward the signal that kills this
# script (a caller's outer time cap, ctrl-c): the container keeps running its
# host processes for hours and loads the box. Name it and remove it on exit.
cleanup() { docker rm -f "aft-opencode2-$RUN_ID" >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM
docker run "${run_args[@]}" "$IMAGE" "$@"
