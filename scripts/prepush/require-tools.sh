#!/usr/bin/env bash
# Report missing executables or unusable tool installations before expensive work.
set -euo pipefail
runner=${1:?runner required}
shift
if [[ "${1:-}" == --probe ]]; then
  name=${2:?tool name required}
  shift 2
  if output=$("$@" 2>&1); then
    printf '%s\n' "$output"
  else
    printf 'PREREQUISITE FAILED: runner=%s required tool unavailable: %s\n' "$runner" "$name" >&2
    exit 127
  fi
  exit 0
fi
for tool in "$@"; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    printf 'PREREQUISITE FAILED: runner=%s missing required tool: %s\n' "$runner" "$tool" >&2
    exit 127
  fi
done
printf 'Required tools available: runner=%s (%s)\n' "$runner" "$*"
