#!/usr/bin/env bash
# Operator tooling runs through the real GitHub CLI (gh); the shim is only for AI agent commands.
# Watch a CI run and FAIL FAST: exit the moment any job concludes 'failure',
# without waiting for the rest of the run. Exit 0 only when the whole run
# succeeds. Prints the first failing job's failed-test lines on the way out.
#
# WATCH_CI_SETTLE=1: on failure, keep waiting until the RUN completes before
# exiting nonzero. Chains that intend to `"$OPERATOR_GH" run rerun --failed`
# need this - a rerun request against a still-running run is refused ("cannot
# be rerun; This workflow is already running"), which has burned rerun-then-watch
# chains twice. Fail-fast reporting still prints immediately; only the exit
# is deferred to rerun-safety.
#
# Usage:
#   scripts/watch-ci.sh            # run for the local HEAD sha
#   scripts/watch-ci.sh <run-id>
#   scripts/watch-ci.sh <sha>      # run for any branch head, e.g. a train branch
#   WATCH_CI_SETTLE=1 scripts/watch-ci.sh <run-id>
set -uo pipefail

# The repository the runs live in: from the checkout's origin unless REPO is
# given. A fixed default was the first thing a lift of this script kept by
# accident, so a lifted copy watched THIS repository's runs for another repo's
# trains. Both github.com URL forms are accepted; anything else refuses so the
# watch never quietly targets the wrong repository.
repo_from_origin() {
  local url
  url="$(git config --get remote.origin.url 2>/dev/null)" || return 1
  case "$url" in
    git@github.com:*) url="${url#git@github.com:}" ;;
    https://github.com/*) url="${url#https://github.com/}" ;;
    ssh://git@github.com/*) url="${url#ssh://git@github.com/}" ;;
    *) return 1 ;;
  esac
  printf '%s\n' "${url%.git}"
}
if [ -z "${REPO:-}" ]; then
  REPO="$(repo_from_origin)" || {
    echo "watch-ci: cannot derive the repository from origin; set REPO=owner/name" >&2
    exit 2
  }
fi
# Which workflow gates a landing. A sha can carry runs from several workflows
# (cost-gate, testbox), so resolving a run BY SHA has to name the gating one or
# it can latch a run that says nothing about the tests.
WORKFLOW="${WATCH_CI_WORKFLOW:-tests.yml}"
# How long to wait for a run to appear for a sha: 40 tries, 15s apart, is ten
# minutes of patience for a queue that normally produces a run in seconds. Both
# knobs exist so tests can drive the resolver without waiting out that budget.
RESOLVE_ATTEMPTS="${WATCH_CI_RESOLVE_ATTEMPTS:-40}"
RESOLVE_SLEEP="${WATCH_CI_RESOLVE_SLEEP:-15}"
ARG="${1:-}"
RID=""
WATCH_SHA=""
# The only positional is a numeric run id or a commit sha. A flag-shaped or
# otherwise unrecognized arg (e.g. a misremembered --sha invocation) would
# otherwise become the "run id", drive `"$OPERATOR_GH" run view` into
# poll-error, and spin this watch forever - hanging any chain that expects it
# to exit and notify.
#
# Run ids are decimal and around 11 digits; a sha is 7-40 hex characters. The
# two only overlap for an all-decimal sha, so length decides that case: a
# 32-or-longer all-decimal string is a sha, never a run id.
#
# Anything that is not a run id is resolved through git rather than pattern
# matched: the run lookup below compares against the FULL head sha, so a short
# sha stored verbatim never matches and the watch reports "no run appeared"
# after the whole poll budget. Resolving also admits tags, branch names and
# HEAD~n, and dereferences an annotated tag to the commit that carries runs.
if [ -n "$ARG" ]; then
  if [[ "$ARG" =~ ^[0-9]+$ ]] && [ "${#ARG}" -lt 32 ]; then
    RID="$ARG"
  elif WATCH_SHA=$(git rev-parse --verify --quiet "${ARG}^{commit}"); then
    :
  else
    echo "watch-ci: argument must be a numeric run id or a commit ref this checkout can resolve (got '$ARG'); pass nothing to watch HEAD's run" >&2
    exit 2
  fi
fi

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/operator-gh.sh" || exit 1

if [ -z "$RID" ]; then
  # Grabbing the newest run right after a push races run creation and latches
  # a stale (often already-failed) run. Resolve the run FOR A SPECIFIC SHA,
  # polling until it appears.
  #
  # The sha, not the branch, is what identifies the run: watching by branch
  # would follow whatever lands there next, and a train is watched on its own
  # branch head rather than on main.
  if [ -z "$WATCH_SHA" ]; then
    WATCH_SHA=$(git rev-parse HEAD 2>/dev/null || echo "")
  fi
  for _ in $(seq 1 "$RESOLVE_ATTEMPTS"); do
    RID=$("$OPERATOR_GH" run list --repo "$REPO" --workflow "$WORKFLOW" --limit 40 \
      --json databaseId,headSha \
      --jq ".[] | select(.headSha==\"$WATCH_SHA\") | .databaseId" | head -1)
    [ -n "$RID" ] && break
    sleep "$RESOLVE_SLEEP"
  done
  if [ -z "$RID" ]; then
    echo "no $WORKFLOW run appeared for $WATCH_SHA" >&2
    exit 2
  fi
fi
echo "watching run $RID (fail-fast)"

# Print the URL on a machine-greppable line: callers that wrap this watch
# (train-push.sh) report the run to the operator without a second gh query.
RUN_URL=$("$OPERATOR_GH" run view "$RID" --repo "$REPO" --json url --jq '.url' 2>/dev/null || echo "")
if [ -n "$RUN_URL" ] && [ "$RUN_URL" != "null" ]; then
  echo "CI_RUN_URL $RUN_URL"
fi

while true; do
  STATUS=$("$OPERATOR_GH" run view "$RID" --repo "$REPO" --json status --jq '.status' 2>/dev/null || echo poll-error)
  # 'Bash permission e2e (Windows)' is continue-on-error in PR mode
  # (_unit-suite.yml strict=false): its job-level conclusion still reads
  # 'failure' in the API, but it does not gate the run. Fail-fast must not
  # fire on it; the run-level conclusion check below remains authoritative.
  FAILED_JOB=$("$OPERATOR_GH" run view "$RID" --repo "$REPO" --json jobs \
    --jq '[.jobs[] | select(.conclusion=="failure") | select(.name | contains("Bash permission") | not)][0] | if . == null then "" else .name + "|" + (.databaseId|tostring) end' 2>/dev/null || echo "")

  if [ -n "$FAILED_JOB" ] && [ "$FAILED_JOB" != "null" ]; then
    NAME="${FAILED_JOB%%|*}"; JID="${FAILED_JOB##*|}"
    echo "CI_EARLY_FAIL job='$NAME' run=$RID"
    "$OPERATOR_GH" run view --repo "$REPO" --job "$JID" --log-failed 2>/dev/null \
      | grep -aE "FAIL \[|panicked at|error\[|bash startup failure" | head -8
    if [ "${WATCH_CI_SETTLE:-0}" = "1" ]; then
      echo "settling: waiting for run completion so a rerun is accepted"
      while [ "$("$OPERATOR_GH" run view "$RID" --repo "$REPO" --json status --jq '.status' 2>/dev/null || echo poll-error)" != "completed" ]; do
        sleep 45
      done
    fi
    exit 1
  fi

  if [ "$STATUS" = "completed" ]; then
    CONC=$("$OPERATOR_GH" run view "$RID" --repo "$REPO" --json conclusion --jq '.conclusion')
    echo "CI_DONE run=$RID conclusion=$CONC"
    [ "$CONC" = "success" ] && exit 0 || exit 1
  fi

  sleep 45
done
