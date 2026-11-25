#!/usr/bin/env bash
set -euo pipefail
umask 077

# nextest invokes the Cargo runner separately for each test process. Isolate the
# embedded daemon too, not just the AFT children whose fixture sets HOME/XDG.
root="${AFT_REAL_DAEMON_RUN_ROOT:?set AFT_REAL_DAEMON_RUN_ROOT}"
mkdir -p "$root"
fixture="$(mktemp -d "$root/test.XXXXXXXX")"
keep=0
cleanup() {
  if (( keep )); then
    python3 "$(dirname "${BASH_SOURCE[0]}")/ci-real-daemon-diagnostics.py" case \
      "$fixture" "$AFT_REAL_DAEMON_REPORTS" || echo 'Could not collect real-daemon case diagnostics' >&2
  fi
  rm -rf "$fixture"
}
trap cleanup EXIT

export HOME="$fixture/home"
export XDG_RUNTIME_DIR="$fixture/runtime"
export XDG_CONFIG_HOME="$fixture/config"
export XDG_DATA_HOME="$fixture/data"
export XDG_STATE_HOME="$fixture/state"
export XDG_CACHE_HOME="$fixture/cache"
export TMPDIR="$fixture/tmp"
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" \
  "$XDG_STATE_HOME" "$XDG_CACHE_HOME" "$TMPDIR"
chmod 700 "$XDG_RUNTIME_DIR"

if [[ -n ${AFT_REAL_DAEMON_REPORTS:-} ]]; then
  for arg in "$@"; do
    if [[ "$arg" =~ ^tool_provider_subc_e2e::([a-z0-9_]{1,100})$ ]]; then
      # Retain the case's logs until cleanup writes the normalized report.
      # Discovery commands do not need any report.
      keep=1
      export AFT_REAL_DAEMON_CASE_DIR="$fixture" AFT_TEST_SUBC_STARTUP_TRACE=1
      printf '%s\n' "${BASH_REMATCH[1]}" > "$fixture/case-name"
      printf '%s test_start\n' "$(date +%s%3N)" > "$fixture/timeline"
      rc=0
      "$@" 2> "$fixture/test.stderr" || rc=$?
      printf '%s test_exit %d\n' "$(date +%s%3N)" "$rc" >> "$fixture/timeline"
      exit "$rc"
    fi
  done
fi
"$@"
