#!/usr/bin/env bash
#
# Content-class arms for the semantic embed RSS harness (issue #327).
#
# Every earlier round of this investigation held corpus content still: uniform
# ASCII Rust files, against a stub backend that accepted every row. The tree
# that reports unbounded daemon memory is neither — it is Cyrillic, dense XML
# and base64 literals, served by a llama.cpp server that refuses rows past its
# context window. Content is therefore the variable these arms move, with file
# count, symbol count, batch size, backend latency and tool-call load held
# exactly where the earlier arms had them, so a difference between two arms is
# attributable to the one thing that changed.
#
# The arms fall into three groups:
#
#   recovery  the backend enforces a context limit, so most batches take the
#             recursive bisection and row-shrinking path. This is the shape no
#             earlier arm ever ran, and it is deliberately run first.
#   content   the backend accepts everything, so the arm measures what the
#             content class alone costs the chunker and the embed loop.
#   sidecar   files carrying extensions the semantic index does not accept.
#             They never become chunks, so they isolate the planes that run
#             beside the embed build from the embed build itself.
#
# Usage:
#   semantic-embed-rss-content-arms.sh --binary <path to aft> [--out DIR]
#                                      [--files N] [--symbols N] [--arms a,b,c]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

BINARY=""
OUT_DIR="$REPO_ROOT/.tmp/semantic-embed-rss-content"
FILES=1200
SYMBOLS=20
DELAY_MS=300
INTERVAL=5
CALLS=1
SECONDS_CAP=900
IDLE_TAIL=20
ARMS=""
SMAPS=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --binary) BINARY="$2"; shift 2 ;;
    --out) OUT_DIR="$2"; shift 2 ;;
    --files) FILES="$2"; shift 2 ;;
    --symbols) SYMBOLS="$2"; shift 2 ;;
    --delay-ms) DELAY_MS="$2"; shift 2 ;;
    --arms) ARMS="$2"; shift 2 ;;
    --smaps) SMAPS="--smaps"; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

[[ -n "$BINARY" ]] || { echo "--binary is required" >&2; exit 2; }

# Each arm is "label|extra harness arguments". The recovery arms come first
# because they test the one mechanism that is both untested and predicted to
# allocate more when more rows are oversized.
#
# Their token limits sit below what each corpus produces: the question those
# arms answer is what the recovery path costs when most batches reach it, not
# whether one synthetic corpus happens to cross a particular server's window.
ALL_ARMS=(
  "cyrillic-recovery|--corpus cyrillic --reject-over-tokens 320"
  "java-recovery|--corpus java --reject-over-tokens 150"
  "rust-baseline|--corpus rust"
  "java-ascii|--corpus java"
  "cyrillic|--corpus cyrillic"
  "xml|--corpus xml"
  "base64|--corpus base64"
  "mixed|--corpus mixed"
  "sidecar-mixed|--corpus java --sidecars mixed --sidecar-ratio 2.0"
)

mkdir -p "$OUT_DIR"
SUMMARY="$OUT_DIR/summary.txt"
: > "$SUMMARY"

for entry in "${ALL_ARMS[@]}"; do
  label="${entry%%|*}"
  extra="${entry#*|}"

  if [[ -n "$ARMS" && ",$ARMS," != *",$label,"* ]]; then
    continue
  fi

  printf '\n=== arm %s ===\n' "$label" | tee -a "$SUMMARY"
  # shellcheck disable=SC2086 -- the extra arguments are intentionally split.
  python3 "$SCRIPT_DIR/semantic-embed-rss.py" \
    --binary "$BINARY" \
    --workdir "$OUT_DIR/$label" \
    --files "$FILES" \
    --symbols "$SYMBOLS" \
    --delay-ms "$DELAY_MS" \
    --interval "$INTERVAL" \
    --calls-per-second "$CALLS" \
    --seconds "$SECONDS_CAP" \
    --stop-after-idle-s "$IDLE_TAIL" \
    --label "$label" \
    $SMAPS \
    $extra 2>&1 | tee -a "$SUMMARY"

  # The corpora are large and there are nine of them; keeping every arm's tree
  # on disk would cost tens of gigabytes for files no reading of the results
  # needs. The samples and the daemon log stay.
  rm -rf "$OUT_DIR/$label/project" "$OUT_DIR/$label/storage"
done

printf '\n=== slopes ===\n' | tee -a "$SUMMARY"
grep -h "SLOPE\|ROWS\|corpus=" "$SUMMARY" | tee -a "$OUT_DIR/slopes.txt"
