#!/usr/bin/env bash
#
# Run the semantic embed RSS harness inside a Linux container.
#
# The reported unbounded-memory builds (issue #327) happen on Linux in a
# container, and several paths the daemon takes there have no macOS equivalent:
# the inotify watcher, the cgroup CPU-quota reader, and /proc-based memory
# accounting. A harness that only ever runs on the developer's laptop cannot
# reach any of them, so this stands the daemon up on real Linux and measures
# VmRSS from /proc/<pid>/status the way the reporter did.
#
# The container is given a CPU quota on purpose. Without one, /sys/fs/cgroup/
# cpu.max reads "max" and the quota branch of the thread-count derivation never
# executes, which would leave one of the three Linux-only paths untested.
#
# The binary is built once with tests/docker/Dockerfile.build-linux and staged
# into the runtime image, rather than compiled inside it. Under amd64 emulation
# on an arm64 host, compiling twice is the difference between minutes and
# tens of minutes.
#
# Usage:
#   semantic-embed-rss-linux.sh [--platform linux/amd64] [--profile dev] \
#       [--cpus 6] [--out DIR] -- [harness args...]
#
# Everything after `--` goes to semantic-embed-rss.py unchanged.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

PLATFORM="linux/amd64"
PROFILE="dev"
CPUS="6"
OUT_DIR="$REPO_ROOT/.tmp/semantic-embed-rss-linux"
BUILD_IMAGE="aft-embed-rss-build"
RUN_IMAGE="aft-embed-rss-run"
REUSE_BINARY=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --platform) PLATFORM="$2"; shift 2 ;;
    --profile) PROFILE="$2"; shift 2 ;;
    --cpus) CPUS="$2"; shift 2 ;;
    --out) OUT_DIR="$2"; shift 2 ;;
    --reuse-binary) REUSE_BINARY="$2"; shift 2 ;;
    --) shift; break ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 2; }

STAGE_DIR="$(mktemp -d)"
trap 'rm -rf "$STAGE_DIR"' EXIT
mkdir -p "$STAGE_DIR/artifact"

if [[ -n "$REUSE_BINARY" ]]; then
  printf 'Staging prebuilt binary from %s\n' "$REUSE_BINARY"
  cp "$REUSE_BINARY" "$STAGE_DIR/artifact/aft"
else
  printf 'Building the %s binary for %s (profile %s)...\n' "aft" "$PLATFORM" "$PROFILE"
  docker build \
    --platform "$PLATFORM" \
    --build-arg "CARGO_PROFILE=$PROFILE" \
    -f "$REPO_ROOT/tests/docker/Dockerfile.build-linux" \
    -t "$BUILD_IMAGE" \
    "$REPO_ROOT"

  # `cargo build --profile dev` lands in target/debug, every other profile in
  # target/<profile>.
  if [[ "$PROFILE" == "dev" ]]; then
    BUILT_PATH="/build/target/debug/aft"
  else
    BUILT_PATH="/build/target/$PROFILE/aft"
  fi
  container="$(docker create --platform "$PLATFORM" "$BUILD_IMAGE")"
  docker cp "$container:$BUILT_PATH" "$STAGE_DIR/artifact/aft"
  docker rm -f "$container" >/dev/null
fi

cp "$SCRIPT_DIR/semantic-embed-rss.py" "$STAGE_DIR/semantic-embed-rss.py"
cp "$SCRIPT_DIR/semantic-embed-rss-linux.Dockerfile" "$STAGE_DIR/Dockerfile"

printf 'Building the runtime image...\n'
docker build --platform "$PLATFORM" -t "$RUN_IMAGE" "$STAGE_DIR"

mkdir -p "$OUT_DIR"
printf 'Running the harness (cpus=%s, results in %s)...\n' "$CPUS" "$OUT_DIR"

# --cpus sets a cgroup v2 CPU quota, which is what makes the container look
# like the reporter's to the daemon's thread-count derivation.
docker run --rm \
  --platform "$PLATFORM" \
  --cpus "$CPUS" \
  -v "$OUT_DIR:/results" \
  "$RUN_IMAGE" \
  --binary /usr/local/bin/aft \
  --workdir /results/run \
  "$@"
