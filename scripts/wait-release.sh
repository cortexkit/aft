#!/usr/bin/env bash
# Wait for the GitHub Actions release workflow to complete for a given tag.
# Usage: ./scripts/wait-release.sh v0.7.5 [--max-wait <seconds>]
# Polls every 5 seconds, exits 0 on success, 1 on failure, 2 on timeout.
#
# Notes for callers wrapping this in another tool (e.g. AI agent bash):
#   - Output uses plain newlines (no `\r` carriage-return tricks) so that
#     line-buffered pipes flush every status update. The wrapper sees the
#     script terminate immediately when the workflow reaches a terminal
#     state — no bash-tool-timeout waiting required.
#   - Default max-wait is 25 minutes (1500s). Healthy releases finish in
#     ~13 min; 25 min gives headroom for slow CI runners without false-
#     cancelling a healthy-but-slow run. Override with --max-wait or set
#     MAX_WAIT_SECONDS env var. The script still exits early on any
#     terminal state regardless of timeout.

set -euo pipefail

TAG=""
MAX_WAIT="${MAX_WAIT_SECONDS:-1500}"
REPO="cortexkit/aft"
INTERVAL=5

while [[ $# -gt 0 ]]; do
  case "$1" in
    --max-wait)
      MAX_WAIT="$2"
      shift 2
      ;;
    -*)
      echo "Unknown flag: $1" >&2
      exit 64
      ;;
    *)
      if [[ -z "$TAG" ]]; then
        TAG="$1"
      else
        echo "Unexpected positional argument: $1" >&2
        exit 64
      fi
      shift
      ;;
  esac
done

if [[ -z "$TAG" ]]; then
  echo "Usage: wait-release.sh <tag> [--max-wait <seconds>]" >&2
  exit 64
fi

echo "⏳ Waiting for release workflow on ${TAG} (max ${MAX_WAIT}s)..."

if ! GH_AUTH_OUTPUT=$(gh auth status --hostname github.com 2>&1); then
  echo "ERROR: gh auth status failed:" >&2
  echo "$GH_AUTH_OUTPUT" >&2
  exit 1
fi

START_TIME=$(date +%s)
GH_RUN_LIST_FAILURES=0
MAX_GH_RUN_LIST_FAILURES=3

gh_run_list() {
  gh run list --repo "$REPO" --workflow release.yml --branch "$TAG" --limit 1 --json status,conclusion,databaseId
}

while true; do
  NOW=$(date +%s)
  ELAPSED=$((NOW - START_TIME))
  if (( ELAPSED >= MAX_WAIT )); then
    echo "⏱  Timed out after ${ELAPSED}s waiting for workflow to complete."
    echo "   Check: https://github.com/${REPO}/actions"
    exit 2
  fi

  # Get the latest run for the release workflow triggered by this tag
  if ! RUN_JSON=$(gh_run_list 2>&1); then
    GH_RUN_LIST_FAILURES=$((GH_RUN_LIST_FAILURES + 1))
    echo "ERROR: gh run list failed (attempt ${GH_RUN_LIST_FAILURES}/${MAX_GH_RUN_LIST_FAILURES}):" >&2
    echo "$RUN_JSON" >&2
    if (( GH_RUN_LIST_FAILURES >= MAX_GH_RUN_LIST_FAILURES )); then
      exit 1
    fi
    sleep "$INTERVAL"
    continue
  fi
  GH_RUN_LIST_FAILURES=0

  STATUS=$(echo "$RUN_JSON" | jq -r '.[0].status // "not_found"')
  CONCLUSION=$(echo "$RUN_JSON" | jq -r '.[0].conclusion // ""')
  RUN_ID=$(echo "$RUN_JSON" | jq -r '.[0].databaseId // ""')

  if [ "$STATUS" = "not_found" ]; then
    echo "  [+${ELAPSED}s] Workflow not started yet..."
    sleep "$INTERVAL"
    continue
  fi

  if [ "$STATUS" = "completed" ]; then
    if [ "$CONCLUSION" = "success" ]; then
      echo "✅ Release workflow succeeded (run ${RUN_ID})"
      echo "   https://github.com/${REPO}/actions/runs/${RUN_ID}"
      exit 0
    else
      echo "❌ Release workflow failed: conclusion=${CONCLUSION} (run ${RUN_ID})"
      echo "   https://github.com/${REPO}/actions/runs/${RUN_ID}"
      exit 1
    fi
  fi

  echo "  [+${ELAPSED}s] Status: ${STATUS} (run ${RUN_ID})"
  sleep "$INTERVAL"
done
