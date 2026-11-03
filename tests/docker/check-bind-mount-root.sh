#!/usr/bin/env bash
# Opens a Docker bind-mount target as the project root, the usual dev-container
# layout (`docker run -v "$PWD":/workspace`), and checks that AFT does not
# refuse it as too broad: the watcher must start and the search index must not
# be disabled. The same container then opens `/`, which must still be refused
# (GitHub issue #670).
#
# The bind mount gives /workspace its own device number, so this needs a real
# kernel mount table; unit tests can only feed the parser fixture text.
#
# Usage: tests/docker/check-bind-mount-root.sh <path to a Linux aft binary>
# Environment:
#   AFT_BIND_CHECK_IMAGE    container image (default ubuntu:22.04, whose glibc
#                           matches the CI build); it needs only sh and timeout
#   AFT_BIND_CHECK_WORKDIR  folder in which to create this run's own temporary
#                           folder (default: $RUNNER_TEMP, $TMPDIR or /tmp); only
#                           that new folder is removed afterwards
set -euo pipefail

binary="${1:?usage: check-bind-mount-root.sh <path to a Linux aft binary>}"
image="${AFT_BIND_CHECK_IMAGE:-ubuntu:22.04}"
command -v docker >/dev/null 2>&1 || { echo "docker is required" >&2; exit 2; }
command -v jq >/dev/null 2>&1 || { echo "jq is required" >&2; exit 2; }
[[ -x "$binary" ]] || { echo "not an executable: $binary" >&2; exit 2; }
binary="$(cd "$(dirname "$binary")" && pwd)/$(basename "$binary")"

# The run always works in a new folder of its own, so a folder the caller names
# is never emptied or removed; only the new folder is cleaned up.
parent="${AFT_BIND_CHECK_WORKDIR:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}}"
mkdir -p "$parent"
work="$(mktemp -d "$parent/aft-bind-root.XXXXXX")"
trap 'rm -rf "$work" 2>/dev/null || true' EXIT
work="$(cd "$work" && pwd)"
# The project is a subfolder of the host folder, so the mount exposes a
# subdirectory of the host filesystem, exactly like `-v "$PWD":/workspace`.
mkdir -p "$work/host/project" "$work/out"
printf 'fn main() {}\n' > "$work/host/project/main.rs"
printf '[package]\nname = "bind-root-check"\nversion = "0.1.0"\nedition = "2021"\n\n[[bin]]\nname = "bind-root-check"\npath = "main.rs"\n' > "$work/host/project/Cargo.toml"

# For each root: configure, ask for status, and keep stdin open briefly so the
# watcher has started (or been refused) before AFT sees end of input and exits.
# A storage folder inside the container keeps every run disposable.
# shellcheck disable=SC2016
container_script='
set -eu
grep " /workspace " /proc/self/mountinfo > /out/workspace-mountinfo.txt || true
stat -c "%d %n" / /workspace > /out/devices.txt
for name in workspace filesystem-root; do
  if [ "$name" = workspace ]; then root=/workspace; else root=/; fi
  {
    printf "%s\n" "{\"id\":\"configure\",\"command\":\"configure\",\"params\":{\"harness\":\"opencode\",\"session\":\"bind-check\",\"project_root\":\"$root\",\"storage_dir\":\"/tmp/aft-storage-$name\"}}"
    printf "%s\n" "{\"id\":\"status\",\"command\":\"status\",\"params\":{}}"
    sleep 3
  } | timeout 120 /aft-bin/aft > "/out/$name.stdout" 2> "/out/$name.stderr"
done
'
docker run --rm \
  --volume "$work/host/project:/workspace" \
  --volume "$binary:/aft-bin/aft:ro" \
  --volume "$work/out:/out" \
  --workdir /workspace \
  "$image" sh -c "$container_script"

echo "/workspace mount: $(cat "$work/out/workspace-mountinfo.txt")"
echo "devices: $(tr '\n' ' ' < "$work/out/devices.txt")"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}
# Prints `true` when the root was refused as too broad by any of the signals
# AFT gives: a configure warning, a degraded status reason, or a disabled
# search index naming the reason.
refused() {
  jq -s '
    (map(select(.id == "configure")) | .[0]) as $configure
    | (map(select(.id == "status")) | .[0]) as $status
    | ($configure.success == true | not) as $configure_failed
    | if $configure_failed or $status == null then "unknown" else
        (($configure.warnings // []) | map(.code) | index("root_too_broad") != null)
        or (($status.degraded_reasons // []) | index("root_too_broad") != null)
        or ($status.search_index.reason == "root_too_broad")
      end
  ' "$work/out/$1.stdout"
}

for name in workspace filesystem-root; do
  echo "--- $name: configure and status replies"
  jq -c 'select(.id == "configure" or .id == "status")
    | {id, success, code, warnings, degraded_reasons, search_index}' "$work/out/$name.stdout" || true
  echo "--- $name: AFT log lines about the root"
  grep -E "root_too_broad|watcher started|project root set" "$work/out/$name.stderr" || true
done

case "$(refused workspace)" in
  false) ;;
  true) fail "the bind-mounted project root /workspace was refused as too broad" ;;
  *) fail "configure or status for /workspace did not reply successfully" ;;
esac
grep -q "watcher started: /workspace" "$work/out/workspace.stderr" \
  || fail "no watcher started for /workspace"
if [[ "$(jq -r 'select(.id == "status") | .search_index.status' "$work/out/workspace.stdout")" == "disabled" ]]; then
  fail "the search index for /workspace is disabled"
fi

case "$(refused filesystem-root)" in
  true) ;;
  false) fail "the filesystem root / was admitted for watching and indexing" ;;
  *) fail "configure or status for / did not reply successfully" ;;
esac
if grep -q "watcher started" "$work/out/filesystem-root.stderr"; then
  fail "a watcher started for /"
fi

if [[ "$failures" -gt 0 ]]; then
  for name in workspace filesystem-root; do
    echo "--- $name: last AFT log lines" >&2
    tail -n 40 "$work/out/$name.stderr" >&2 || true
  done
  echo "bind-mount root check FAILED ($failures problem(s))" >&2
  exit 1
fi
echo "bind-mount root check passed: /workspace is watched and indexed, / is refused"
