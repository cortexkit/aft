#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "$root/target/scratch"
scratch="$(mktemp -d "$root/target/scratch/cmake-preflight.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
bash_bin="$(command -v bash)"
checks=0

check() {
  local label="$1" expected="$2" text="$3"
  mkdir -p "$scratch/$label"
  if [[ "$label" != missing ]]; then
    printf '#!/bin/sh\nprintf "%%s\\n" "%s"\n' "$text" > "$scratch/$label/cmake"
    chmod +x "$scratch/$label/cmake"
  fi
  local actual=0
  PATH="$scratch/$label" "$bash_bin" "$root/scripts/check-cmake.sh" > "$scratch/result" 2>&1 || actual=$?
  if [[ "$actual" != "$expected" ]]; then
    printf 'FAIL %s: expected exit %s, got %s\n' "$label" "$expected" "$actual" >&2
    exit 1
  fi
  if [[ "$expected" != 0 ]]; then
    result="$(< "$scratch/result")"
    [[ "$result" == *"CMake 3.12 or later"* && "$result" == *"wasm-grammars"* ]] || exit 1
  fi
  checks=$((checks + 1))
}

check missing 1 ''
check old 1 'cmake version 3.11.4'
check minimum 0 'cmake version 3.12.0'
check current 0 'cmake version 3.31.6'
check next-major 0 'cmake version 4.4.4'
check invalid 1 'unknown cmake version'
printf 'CMake optional-feature preflight: %s checks passed\n' "$checks"
