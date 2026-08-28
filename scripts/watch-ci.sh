#!/usr/bin/env bash
# Watch a CI run and FAIL FAST: exit the moment any job concludes 'failure',
# without waiting for the rest of the run. Exit 0 only when the whole run
# succeeds. Prints the first failing job's failed-test lines on the way out.
#
# WATCH_CI_SETTLE=1: on failure, keep waiting until the RUN completes before
# exiting nonzero. Chains that intend to `gh run rerun --failed` need this -
# a rerun request against a still-running run is refused ("cannot be rerun;
# This workflow is already running"), which has burned rerun-then-watch
# chains twice. Fail-fast reporting still prints immediately; only the exit
# is deferred to rerun-safety.
#
# Usage:
#   scripts/watch-ci.sh            # newest run on main
#   scripts/watch-ci.sh <run-id>
#   WATCH_CI_SETTLE=1 scripts/watch-ci.sh <run-id>
set -uo pipefail

REPO="${REPO:-cortexkit/aft}"
RID="${1:-}"
# The only positional is a numeric run id. A flag-shaped or non-numeric arg
# (e.g. a misremembered --sha invocation) would otherwise become the "run id",
# drive `gh run view` into poll-error, and spin this watch forever - hanging
# any chain that expects it to exit and notify.
if [ -n "$RID" ] && ! [[ "$RID" =~ ^[0-9]+$ ]]; then
  echo "watch-ci: run id must be numeric (got '$RID'); pass nothing to watch HEAD's run" >&2
  exit 2
fi
if [ -z "$RID" ]; then
  # Grabbing the newest run right after a push races run creation and latches
  # a stale (often already-failed) run. Resolve the run FOR THE LOCAL HEAD SHA,
  # polling until it appears.
  HEAD_SHA=$(git rev-parse HEAD 2>/dev/null || echo "")
  for _ in $(seq 1 40); do
    RID=$(gh run list --repo "$REPO" --branch main --limit 5 \
      --json databaseId,headSha \
      --jq ".[] | select(.headSha==\"$HEAD_SHA\") | .databaseId" | head -1)
    [ -n "$RID" ] && break
    sleep 15
  done
  if [ -z "$RID" ]; then
    echo "no CI run appeared for HEAD $HEAD_SHA" >&2
    exit 2
  fi
fi
echo "watching run $RID (fail-fast)"

while true; do
  STATUS=$(gh run view "$RID" --repo "$REPO" --json status --jq '.status' 2>/dev/null || echo poll-error)
  # 'Bash permission e2e (Windows)' is continue-on-error in PR mode
  # (_unit-suite.yml strict=false): its job-level conclusion still reads
  # 'failure' in the API, but it does not gate the run. Fail-fast must not
  # fire on it; the run-level conclusion check below remains authoritative.
  FAILED_JOB=$(gh run view "$RID" --repo "$REPO" --json jobs \
    --jq '[.jobs[] | select(.conclusion=="failure") | select(.name | contains("Bash permission") | not)][0] | if . == null then "" else .name + "|" + (.databaseId|tostring) end' 2>/dev/null || echo "")

  if [ -n "$FAILED_JOB" ] && [ "$FAILED_JOB" != "null" ]; then
    NAME="${FAILED_JOB%%|*}"; JID="${FAILED_JOB##*|}"
    echo "CI_EARLY_FAIL job='$NAME' run=$RID"
    gh run view --repo "$REPO" --job "$JID" --log-failed 2>/dev/null \
      | grep -aE "FAIL \[|panicked at|error\[|bash startup failure" | head -8
    if [ "${WATCH_CI_SETTLE:-0}" = "1" ]; then
      echo "settling: waiting for run completion so a rerun is accepted"
      while [ "$(gh run view "$RID" --repo "$REPO" --json status --jq '.status' 2>/dev/null || echo poll-error)" != "completed" ]; do
        sleep 45
      done
    fi
    exit 1
  fi

  if [ "$STATUS" = "completed" ]; then
    CONC=$(gh run view "$RID" --repo "$REPO" --json conclusion --jq '.conclusion')
    echo "CI_DONE run=$RID conclusion=$CONC"
    [ "$CONC" = "success" ] && exit 0 || exit 1
  fi

  sleep 45
done
