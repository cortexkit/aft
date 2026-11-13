#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo "CMake 3.12 or later is required to build AFT with --features wasm-grammars; install CMake and put it on PATH." >&2
  exit 1
}

command -v cmake >/dev/null 2>&1 || fail
version="$(cmake --version)" || fail
printf '%s\n' "$version"
if [[ "$version" =~ ([0-9]+)\.([0-9]+) ]]; then
  major="${BASH_REMATCH[1]}"
  minor="${BASH_REMATCH[2]}"
  if (( major > 3 || (major == 3 && minor >= 12) )); then
    exit 0
  fi
fi
fail
