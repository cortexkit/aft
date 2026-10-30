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
  ci_only=()
  if git rev-parse --verify HEAD >/dev/null 2>&1; then
    if [[ -n "${PREPUSH_COMMIT:-}" && "$PREPUSH_COMMIT" != "$(git rev-parse HEAD)" ]]; then
      echo 'Printed lane belongs to a different commit; regenerate --print-lanes after committing.'
      echo "GATE FAILED: $lane provenance"
      return 2
    fi
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

# Record a step this runner cannot run at all, so the report names it instead of
# passing silently. Only for capabilities the runner lacks by design (for example
# containers inside build-server job VMs); a missing tool that could be installed
# must stay a prerequisite failure. GitHub CI remains the gate for these steps.
prepush_ci_only() {
  local name=$1 reason=$2
  printf '\n== %s: CI-ONLY: %s\n' "$name" "$reason" >> "$lane_log"
  printf '%s: ci-only (%s)\n' "$name" "$reason"
  ci_only+=("$name")
}

prepush_finish() {
  # This pipe reads the saved log only; test exit codes were collected above.
  # Keep failure reporting usable even when Python itself is the missing tool.
  grep -E 'Summary \[|^test result:|^Ran [0-9]+ tests|^[[:space:]]*[0-9]+ (pass|fail)$|Checked [0-9]+ files' "$lane_log" | tail -n 25 || true
  printf '%s elapsed=%ss log=%s\n' "$lane" "$((SECONDS - lane_start))" "$lane_log"
  if ((${#ci_only[@]})); then
    printf 'NOT RUN HERE (GitHub CI only):'
    printf ' %s' "${ci_only[@]}"
    printf '\n'
  fi
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
