#!/usr/bin/env bash
set -euo pipefail
# Connection-file admission rejects group-writable parent directories.
umask 077

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
base="${RUNNER_TEMP:-$repo/target/ci-real-daemon}"
mkdir -p "$base"
run_root="$(mktemp -d "$base/aft-real-daemon.XXXXXXXX")"
chmod 700 "$run_root"
heartbeat=""
cleanup() {
  if [[ -n "$heartbeat" ]]; then
    kill "$heartbeat" 2>/dev/null || true
    wait "$heartbeat" 2>/dev/null || true
  fi
  rm -rf "$run_root"
}
trap cleanup EXIT

export TMPDIR="$run_root/tmp"
mkdir -p "$TMPDIR"
printf '==> Daemon fixture directory ancestry (mode owner:group path):\n'
ancestor="$TMPDIR"
insecure=0
while :; do
  stat -L -c '%a %u:%g %n' "$ancestor"
  mode="$(stat -L -c '%a' "$ancestor")"
  # Sticky system temp directories are safe; writable non-sticky parents are not.
  if (( (8#$mode & 0022) != 0 && (8#$mode & 01000) == 0 )); then
    printf 'Insecure daemon fixture ancestor: %s (mode %s)\n' "$ancestor" "$mode" >&2
    insecure=1
  fi
  [[ "$ancestor" == / ]] && break
  ancestor="$(dirname "$ancestor")"
done
if (( insecure != 0 )); then
  exit 1
fi

log="$run_root/real-daemon-tests.log"
export AFT_REAL_DAEMON_RUN_ROOT="$TMPDIR/real-daemon-homes"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="bash $repo/scripts/ci-real-daemon-test-runner.sh"

cargo nextest --version
printf '==> Building and running ignored real-daemon tool-provider tests\n'
: > "$log"
(
  while sleep 60; do
    printf '==> Real-daemon gate still running; recent output:\n'
    tail -n 5 "$log"
  done
) &
heartbeat=$!

# The real daemon is the dev-only subc-daemon dependency pinned in Cargo.toml.
# fetch-subc-core.sh supplies a different daemon for the plugin suites, not these tests.
rc=0
timeout 15m cargo nextest run -p agent-file-tools --test rest \
  --features test-hooks,test-scope-owner --profile gate --no-fail-fast \
  --run-ignored only --test-threads 2 --failure-output final --final-status-level all \
  -E 'test(/^tool_provider_subc_e2e::tool_provider_subc_e2e_/)' > "$log" 2>&1 || rc=$?
if (( rc != 0 )); then
  printf '==> Real-daemon gate FAILED (rc=%s); failing test names:\n' "$rc" >&2
  grep -E '(FAIL|TIMEOUT|LEAK) \[' "$log" >&2 || true
fi
tail -n 60 "$log"
exit "$rc"
