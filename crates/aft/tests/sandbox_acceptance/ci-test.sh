#!/usr/bin/env bash
set -euo pipefail

log=$(mktemp)
trap 'rm -f "$log"' EXIT
status=0
"$@" >"$log" 2>&1 || status=$?
cat "$log"
if [[ "$status" != 0 ]]; then
  exit "$status"
fi
if ! grep -Eq '^test result: ok\. [1-9][0-9]* passed;' "$log"; then
  printf 'No tests passed: %s\n' "$*" >&2
  exit 1
fi
if grep -Eq 'skipped active (bubblewrap|composition) oracle' "$log"; then
  printf 'An active-backend probe skipped: %s\n' "$*" >&2
  exit 1
fi
