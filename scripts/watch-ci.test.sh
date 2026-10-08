#!/usr/bin/env bash
# Deterministic polling tests for watch-ci.sh. Fake gh responses and sleep calls
# make API usage and backoff observable without network access or wall-clock waits.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WATCH_CI="$SCRIPT_DIR/watch-ci.sh"
REAL_JQ="$(command -v jq || true)"
if [ -z "$REAL_JQ" ]; then
  echo 'watch-ci.test.sh: jq is required' >&2
  exit 2
fi
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/watch-ci-test.XXXXXX")"
trap 'rm -rf "$TMP_ROOT"' EXIT

BIN="$TMP_ROOT/bin"
STATE="$TMP_ROOT/state"
REPO="$TMP_ROOT/repo"
mkdir -p "$BIN" "$STATE/responses"
git init -q "$REPO"
git -C "$REPO" config remote.origin.url https://github.com/example/repo.git
git -C "$REPO" config user.name 'Watcher Test'
git -C "$REPO" config user.email 'watcher@example.invalid'

cat > "$BIN/gh" <<'GH'
#!/usr/bin/env bash
set -u
STATE="${WATCH_CI_TEST_STATE:?}"
printf '%s\n' "$*" >> "$STATE/calls"
count=0
[ ! -f "$STATE/calls" ] || count="$(wc -l < "$STATE/calls" | tr -d ' ')"
response="$STATE/responses/$count"
[ -f "$response" ] || { echo "unexpected gh query $count: $*" >&2; exit 90; }
if [ "$(head -1 "$response")" = ERROR ]; then
  exit 1
fi
cat "$response"
GH
cat > "$BIN/sleep" <<'SLEEP'
#!/usr/bin/env bash
printf '%s\n' "$1" >> "${WATCH_CI_TEST_STATE:?}/sleeps"
SLEEP
cat > "$BIN/jq" <<'JQ'
#!/usr/bin/env bash
case "${WATCH_CI_TEST_FAIL_JQ:-}" in
  gating)
    for arg in "$@"; do
      case "$arg" in
        *'join("; ")'*) echo 'injected gating jq failure' >&2; exit 42 ;;
      esac
    done
    ;;
  failed-job)
    for arg in "$@"; do
      case "$arg" in
        *'select(.conclusion == "failure")'*) echo 'injected failed-job jq failure' >&2; exit 42 ;;
      esac
    done
    ;;
esac
exec "${WATCH_CI_TEST_REAL_JQ:?}" "$@"
JQ
chmod +x "$BIN/gh" "$BIN/sleep" "$BIN/jq"

checks=0
failures=0
ok() { checks=$((checks + 1)); printf 'watch-ci.test.sh: ok — %s\n' "$1"; }
fail() { printf 'watch-ci.test.sh: FAIL — %s\n' "$1" >&2; failures=$((failures + 1)); }

reset_case() {
  rm -f "$STATE/calls" "$STATE/sleeps" "$STATE/responses"/* "$TMP_ROOT/output"
}

response() {
  local index="$1"
  shift
  printf '%s\n' "$*" > "$STATE/responses/$index"
}

api_error() { printf 'ERROR\n' > "$STATE/responses/$1"; }

run_watch() {
  local poll_sleep="${1:-1}" max_sleep="${2:-8}" fail_jq="${3:-}"
  set +e
  (cd "$REPO" && PATH="$BIN:$PATH" WATCH_CI_TEST_STATE="$STATE" \
    WATCH_CI_TEST_REAL_JQ="$REAL_JQ" WATCH_CI_TEST_FAIL_JQ="$fail_jq" \
    OPERATOR_GH_FALLBACK_PATHS="$BIN/gh" WATCH_CI_POLL_SLEEP="$poll_sleep" \
    WATCH_CI_POLL_MAX_SLEEP="$max_sleep" "$WATCH_CI" 4242) \
    > "$TMP_ROOT/output" 2>&1
  WATCH_RC=$?
  set -e
}

completed_success='{"status":"completed","conclusion":"success","jobs":[{"name":"Unit","databaseId":101,"conclusion":"success"}],"url":"https://github.com/example/repo/actions/runs/4242"}'

# A successful completed run needs one combined API response, not separate
# status, URL, and jobs queries.
reset_case
response 1 "$completed_success"
run_watch
calls="$(wc -l < "$STATE/calls" | tr -d ' ')"
if [ "$WATCH_RC" -eq 0 ] && [ "$calls" -eq 1 ] &&
  grep -q -- '--json status,conclusion,jobs,url' "$STATE/calls" &&
  grep -q '^CI_RUN_URL https://github.com/example/repo/actions/runs/4242$' "$TMP_ROOT/output"; then
  ok 'one combined gh query supplies the first completed tick and run URL'
else
  fail 'one combined gh query supplies the first completed tick and run URL'
fi

# Consecutive API errors back off to the cap; a successful response resets the
# delay, including after another isolated API error.
reset_case
api_error 1
api_error 2
api_error 3
response 4 '{"status":"in_progress","conclusion":null,"jobs":[],"url":"https://github.com/example/repo/actions/runs/4242"}'
api_error 5
response 6 "$completed_success"
run_watch 1 4
delays="$(tr '\n' ' ' < "$STATE/sleeps" | sed 's/ $//')"
if [ "$WATCH_RC" -eq 0 ] && [ "$delays" = '1.000 2.000 4.000 1 1.000' ]; then
  ok 'API errors back off with a cap and a success resets the delay'
else
  fail "API errors back off with a cap and a success resets the delay (delays: $delays)"
fi

# A completed payload with an unreadable job list fails closed even when the
# run's conclusion is success.
reset_case
response 1 '{"status":"completed","conclusion":"success","jobs":null,"url":""}'
run_watch
if [ "$WATCH_RC" -eq 3 ] && grep -q 'CI_UNDETERMINED' "$TMP_ROOT/output"; then
  ok 'unreadable completed job list exits 3'
else
  fail 'unreadable completed job list exits 3'
fi

# A completed status with no readable conclusion is equally undetermined,
# even when the jobs are present and individually green.
reset_case
response 1 '{"status":"completed","conclusion":null,"jobs":[{"name":"Unit","databaseId":101,"conclusion":"success"}],"url":""}'
run_watch
if [ "$WATCH_RC" -eq 3 ] && grep -q 'CI_UNDETERMINED' "$TMP_ROOT/output"; then
  ok 'unreadable completed conclusion exits 3'
else
  fail 'unreadable completed conclusion exits 3'
fi

# A successful but empty jobs array is not evidence that the run passed.
reset_case
response 1 '{"status":"completed","conclusion":"success","jobs":[],"url":""}'
run_watch
if [ "$WATCH_RC" -eq 3 ] && grep -q 'CI_UNDETERMINED' "$TMP_ROOT/output"; then
  ok 'empty completed job list exits 3'
else
  fail 'empty completed job list exits 3'
fi

# Keep the watcher's established all-jobs-passed report. train-push's separate
# completed-conclusion guard prevents that report from authorizing a landing.
reset_case
response 1 '{"status":"completed","conclusion":"cancelled","jobs":[{"name":"Unit","databaseId":101,"conclusion":"success"}],"url":""}'
run_watch
if [ "$WATCH_RC" -eq 0 ] && grep -q 'CI_DONE run=4242 conclusion=cancelled jobs_all_passed=1' "$TMP_ROOT/output"; then
  ok 'non-success run retains watcher output while the landing guard stays authoritative'
else
  fail 'non-success run retains watcher output while the landing guard stays authoritative'
fi

# An unnamed failure remains a real red job; it must not disappear from the
# fail-fast query just because the API omitted its display name.
reset_case
response 1 '{"status":"completed","conclusion":"cancelled","jobs":[{"name":null,"databaseId":101,"conclusion":"failure"}],"url":""}'
run_watch
if [ "$WATCH_RC" -eq 1 ] && grep -q "CI_EARLY_FAIL job='<unnamed>' run=4242" "$TMP_ROOT/output"; then
  ok 'failed job with a null name remains fail-fast red'
else
  fail 'failed job with a null name remains fail-fast red'
fi

# Failures in either jq extraction are undetermined, never empty success data.
reset_case
response 1 '{"status":"completed","conclusion":"cancelled","jobs":[{"name":"Unit","databaseId":101,"conclusion":"cancelled"}],"url":""}'
run_watch 1 8 gating
if [ "$WATCH_RC" -eq 3 ] && grep -q 'CI_UNDETERMINED.*could not read job conclusions' "$TMP_ROOT/output"; then
  ok 'a gating-job jq failure exits 3'
else
  fail 'a gating-job jq failure exits 3'
fi

reset_case
response 1 '{"status":"completed","conclusion":"success","jobs":[{"name":"Unit","databaseId":101,"conclusion":"success"}],"url":""}'
run_watch 1 8 failed-job
if [ "$WATCH_RC" -eq 3 ] && grep -q 'CI_UNDETERMINED.*could not read job conclusions' "$TMP_ROOT/output"; then
  ok 'a fail-fast-job jq failure exits 3'
else
  fail 'a fail-fast-job jq failure exits 3'
fi

if [ "$failures" -ne 0 ]; then
  printf 'watch-ci.test.sh: %s check(s) failed\n' "$failures" >&2
  exit 1
fi
printf 'watch-ci.test.sh: passed (%s checks)\n' "$checks"
