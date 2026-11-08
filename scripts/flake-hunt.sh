#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/flake-hunt.sh [--pressure] [--hogs N] <test-filter> <iterations> [-- <extra nextest args>]

Runs one cargo-nextest test-name filter repeatedly, records every iteration's
status and duration, and exits nonzero if any iteration fails.

Options:
  --pressure   Run the hunt while low-priority CPU hogs create contention.
               Intended for the configure ack latency flake.
  --hogs N     Number of pressure hogs to spawn with --pressure (1-4, default 4).
  -h, --help   Show this help.

Environment:
  AFT_FLAKE_HUNT_PACKAGE   Cargo package to test (default: agent-file-tools).
  AFT_FLAKE_HUNT_LOG_DIR   Directory for per-iteration logs and results.tsv
                           (default: target/flake-hunt/<timestamp>-<pid>).
  AFT_FLAKE_HUNT_HOGS      Default hog count for --pressure (max 4).

Examples:
  scripts/flake-hunt.sh subc_bridge_lossy_pressure_reliable_completion_still_delivers 100
  scripts/flake-hunt.sh --pressure configure_defers_large_tree_file_walk_until_after_ack 100
USAGE
}

pressure=0
hogs="${AFT_FLAKE_HUNT_HOGS:-4}"
filter=""
iterations=""
extra_nextest_args=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --pressure)
      pressure=1
      shift
      ;;
    --hogs)
      if [[ $# -lt 2 ]]; then
        echo "error: --hogs requires a value" >&2
        usage >&2
        exit 2
      fi
      hogs="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      extra_nextest_args+=("$@")
      break
      ;;
    *)
      if [[ -z "$filter" ]]; then
        filter="$1"
      elif [[ -z "$iterations" ]]; then
        iterations="$1"
      else
        extra_nextest_args+=("$1")
      fi
      shift
      ;;
  esac
done

if [[ -z "$filter" || -z "$iterations" ]]; then
  echo "error: test-filter and iterations are required" >&2
  usage >&2
  exit 2
fi

if ! [[ "$iterations" =~ ^[1-9][0-9]*$ ]]; then
  echo "error: iterations must be a positive integer: $iterations" >&2
  exit 2
fi

if ! command -v cargo-nextest >/dev/null 2>&1; then
  echo "error: cargo-nextest is required; install it with taiki-e/install-action@nextest in CI or a binary install locally" >&2
  exit 127
fi

if ! [[ "$hogs" =~ ^[0-9]+$ ]]; then
  echo "error: hog count must be an integer: $hogs" >&2
  exit 2
fi

if (( pressure == 1 )); then
  if (( hogs < 1 || hogs > 4 )); then
    echo "error: pressure hog count must be between 1 and 4; got $hogs" >&2
    exit 2
  fi
else
  hogs=0
fi

package="${AFT_FLAKE_HUNT_PACKAGE:-agent-file-tools}"
timestamp="$(date -u '+%Y%m%dT%H%M%SZ')"
log_dir="${AFT_FLAKE_HUNT_LOG_DIR:-target/flake-hunt/${timestamp}-$$}"
mkdir -p "$log_dir"
results_file="$log_dir/results.tsv"

now_ms() {
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import time; print(time.time_ns() // 1_000_000)'
  else
    echo "$(($(date +%s) * 1000))"
  fi
}

hog_marker="aft-flake-hunt-hog-$$-${RANDOM:-0}"
hog_pids=()

cleanup_hogs() {
  if (( ${#hog_pids[@]} > 0 )); then
    kill "${hog_pids[@]}" >/dev/null 2>&1 || true
    wait "${hog_pids[@]}" >/dev/null 2>&1 || true
  fi

  if command -v pgrep >/dev/null 2>&1; then
    local survivors=""
    survivors="$(pgrep -f "$hog_marker" || true)"
    if [[ -n "$survivors" ]]; then
      echo "warning: cleaning surviving pressure hogs: $survivors" >&2
      pkill -f "$hog_marker" >/dev/null 2>&1 || true
      sleep 1
      survivors="$(pgrep -f "$hog_marker" || true)"
      if [[ -n "$survivors" ]]; then
        echo "warning: pressure hog cleanup left survivors: $survivors" >&2
      fi
    fi
  fi
}

handle_signal() {
  cleanup_hogs
  trap - EXIT
  exit 130
}

start_hogs() {
  if (( hogs == 0 )); then
    return
  fi

  echo "starting $hogs low-priority CPU pressure hog(s) with marker $hog_marker"
  for (( i = 1; i <= hogs; i++ )); do
    nice -n 19 bash -c 'while :; do :; done' "$hog_marker-$i" &
    hog_pids+=("$!")
  done

  if command -v pgrep >/dev/null 2>&1; then
    local running
    running="$( (pgrep -f "$hog_marker" || true) | wc -l | tr -d '[:space:]')"
    echo "pressure hogs running: $running"
  fi
}

trap cleanup_hogs EXIT
trap handle_signal INT TERM
start_hogs

printf 'iteration\tstatus\tduration_ms\texit_code\tlog_file\n' | tee "$results_file"

total_start_ms="$(now_ms)"
passes=0
failures=0

for (( iteration = 1; iteration <= iterations; iteration++ )); do
  iter_label="$(printf '%04d' "$iteration")"
  log_file="$log_dir/iteration-${iter_label}.log"
  start_ms="$(now_ms)"

  nextest_cmd=(
    cargo nextest run
    --features test-hooks
    -p "$package"
    --no-fail-fast
    --failure-output immediate-final
    --no-capture
    "$filter"
  )
  if (( ${#extra_nextest_args[@]} > 0 )); then
    nextest_cmd+=("${extra_nextest_args[@]}")
  fi

  printf '==> iteration %s/%s:' "$iteration" "$iterations"
  printf ' %q' "${nextest_cmd[@]}"
  printf '\n'

  set +e
  "${nextest_cmd[@]}" 2>&1 | tee "$log_file"
  status="${PIPESTATUS[0]}"
  set -e

  end_ms="$(now_ms)"
  duration_ms=$((end_ms - start_ms))

  if (( status == 0 )); then
    result="pass"
    ((passes += 1))
  else
    result="fail"
    ((failures += 1))
  fi

  printf '%s\t%s\t%s\t%s\t%s\n' "$iteration" "$result" "$duration_ms" "$status" "$log_file" | tee -a "$results_file"
done

total_end_ms="$(now_ms)"
total_duration_ms=$((total_end_ms - total_start_ms))
failure_rate="$(python3 - "$failures" "$iterations" <<'PY' 2>/dev/null || true
import sys
failures = int(sys.argv[1])
iterations = int(sys.argv[2])
print(f"{(failures / iterations) * 100:.2f}%")
PY
)"
if [[ -z "$failure_rate" ]]; then
  failure_rate="$((failures * 100 / iterations))%"
fi

pass_rate="$(python3 - "$passes" "$iterations" <<'PY' 2>/dev/null || true
import sys
passes = int(sys.argv[1])
iterations = int(sys.argv[2])
print(f"{(passes / iterations) * 100:.2f}%")
PY
)"
if [[ -z "$pass_rate" ]]; then
  pass_rate="$((passes * 100 / iterations))%"
fi

cat <<SUMMARY
==> flake hunt summary
filter: $filter
package: $package
iterations: $iterations
passes: $passes ($pass_rate)
failures: $failures ($failure_rate)
pressure_hogs: $hogs
elapsed_ms: $total_duration_ms
results: $results_file
logs: $log_dir
SUMMARY

if (( failures > 0 )); then
  exit 1
fi
