#!/usr/bin/env bash
# Shared reporting for local and build-server lanes; never turn a failed step green.
set -euo pipefail

prepush_init() {
  lane=$1
  cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
  log_dir=${PREPUSH_LOG_DIR:-$PWD/.cortexkit/internal-docs/investigations/prepush}
  mkdir -p "$log_dir"
  log_dir=$(cd "$log_dir" && pwd)
  lane_log="$log_dir/$lane.log"
  : > "$lane_log"
  lane_start=$SECONDS
  failures=()
  if git rev-parse --verify HEAD >/dev/null 2>&1; then
    git log -1 --format='%H %s'
    git log -1 >> "$lane_log"
  else
    # Some Windows transports omit Git metadata. Record the caller-reported
    # snapshot SHA, which cannot be verified against the copied files here.
    # An empty Git root lets fixture helpers locate the checkout without
    # creating a commit or inventing history.
    [[ "${PREPUSH_COMMIT:-}" =~ ^[0-9a-f]{40}$ ]] || {
      echo 'Source snapshot has no Git history; pass PREPUSH_COMMIT=<snapshot SHA>.'
      echo "GATE FAILED: $lane provenance"
      return 2
    }
    git init -q
    printf 'Source snapshot %s (transport omitted Git history)\n' "$PREPUSH_COMMIT" | tee -a "$lane_log"
  fi
  printf 'lane=%s full log=%s\n' "$lane" "$lane_log"
}

prepush_step() {
  local name=$1 start=$SECONDS rc=0
  shift
  printf '\n== %s: ' "$name" >> "$lane_log"
  printf '%q ' "$@" >> "$lane_log"
  printf '\n' >> "$lane_log"
  # Steps are external commands/scripts, not shell functions: conditionally
  # calling a function would disable its errexit and hide early failures.
  "$@" >> "$lane_log" 2>&1 || rc=$?
  printf '%s: exit=%s (%ss)\n' "$name" "$rc" "$((SECONDS - start))"
  if ((rc != 0)); then
    failures+=("$name")
    tail -n 35 "$lane_log"
    return "$rc"
  fi
}

prepush_finish() {
  python3 - "$lane_log" <<'PY' || failures+=(summary-parser)
import re
import sys
from pathlib import Path
lines = Path(sys.argv[1]).read_text(errors="replace").splitlines()
summaries = [line for line in lines if re.search(r"Summary \[|^test result:|^Ran \d+ tests|^\s*\d+ (pass|fail)$|Checked \d+ files", line)]
for line in summaries[-25:]:
    print(line)
PY
  printf '%s elapsed=%ss log=%s\n' "$lane" "$((SECONDS - lane_start))" "$lane_log"
  if ((${#failures[@]})); then
    printf 'GATE FAILED: %s' "$lane"
    printf ' %s' "${failures[@]}"
    printf '\n'
    return 1
  fi
  echo 'GATE PASSED'
}

prepush_abort() {
  local rc=$1
  if ((rc)); then
    ((${#failures[@]})) || failures+=(setup)
    prepush_finish || true
  fi
}

prepush_require_platform() {
  local expected=$1 actual
  actual=$(uname -s)
  case "$expected:$actual" in
    linux:Linux|macos:Darwin|windows:MINGW*|windows:MSYS*|windows:CYGWIN*) ;;
    *) echo "This lane requires runon $expected, not $actual."; return 2 ;;
  esac
}
