#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HARNESS_DIR="$SCRIPT_DIR/opencode2/harness"
# The repository the harness tags its images under. The tag is derived from
# the inputs the image was built from, so this is a name, not an identity; see
# the build below.
IMAGE_REPOSITORY="${AFT_OPENCODE2_IMAGE:-aft-e2e-opencode2-linux}"
# Every image this script builds carries this label, which is how the reaper
# finds the ones it is allowed to remove. Images built before the label
# existed are not ours to recognise and have to be removed by hand once.
HARNESS_IMAGE_LABEL="org.cortexkit.aft-e2e=opencode2"
# Set once the build has produced one; the reaper does nothing until then.
IMAGE=""
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

# Name the image after the inputs that produced it, not after the run.
#
# A per-run name is how several 4GB images pile up in a day: nothing ever
# supersedes anything. Naming by inputs means a re-run lands on the tag it used
# last time and a changed input lands on a new one, so each build replaces its
# predecessor rather than joining it. The build itself still decides what is in
# the image — an uncommitted edit does not move this key, and does not have to:
# the build runs either way and the tag simply follows whatever came out of it.
# Reaping below is what turns "supersedes" into "replaces".
BINARY_IDENTITY="checkout-build"
if [[ -n "${AFT_BINARY_PATH:-}" ]]; then
  BINARY_IDENTITY="$(tr -d '[:space:]' < "$(cd "$(dirname "$AFT_BINARY_PATH")" && pwd)/build-info.json")"
fi
digest_stdin() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi
}
CONTENT_KEY="$(printf '%s\n' "$GIT_SHA" "$HOST_VERSION" "$V1_HOST_VERSION" "$BINARY_IDENTITY" |
  digest_stdin | cut -c1-12)"
IMAGE="$IMAGE_REPOSITORY:$CONTENT_KEY"

docker build \
  --platform linux/amd64 \
  "${build_args[@]}" \
  --label "$HARNESS_IMAGE_LABEL" \
  --file "$HARNESS_DIR/Dockerfile" \
  --tag "$IMAGE" \
  "$REPO_ROOT"
stage_cleanup
printf 'Harness image: %s\n' "$IMAGE"

# Remove the harness images this one supersedes, so the steady state is one
# image rather than one per run: the other tags in this repository named builds
# this one replaces, and a rebuild onto the same tag leaves its predecessor
# untagged. Nothing is removed forcibly, so an image another run still has a
# container on stays where it is; the narrow case this cannot protect is a
# concurrent run that has built but not yet started its container, which loses
# a rebuild rather than its results.
reap_superseded_images() {
  local ref id
  [[ -z "$IMAGE" ]] && return 0
  while IFS= read -r ref; do
    [[ -z "$ref" || "$ref" == "$IMAGE" || "$ref" == *":<none>" ]] && continue
    docker image rm "$ref" >/dev/null 2>&1 || true
  done < <(docker image ls "$IMAGE_REPOSITORY" --format '{{.Repository}}:{{.Tag}}' 2>/dev/null || true)
  while IFS= read -r id; do
    [[ -z "$id" ]] && continue
    docker image rm "$id" >/dev/null 2>&1 || true
  done < <(docker image ls --all --no-trunc --format '{{.ID}}' \
    --filter "label=$HARNESS_IMAGE_LABEL" --filter "dangling=true" 2>/dev/null || true)
}

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
# The superseded images go the same way, so a killed run reaps what it replaced
# rather than leaving it on disk.
cleanup() {
  docker rm -f "aft-opencode2-$RUN_ID" >/dev/null 2>&1 || true
  reap_superseded_images
}
trap cleanup EXIT INT TERM
docker run "${run_args[@]}" "$IMAGE" "$@"
