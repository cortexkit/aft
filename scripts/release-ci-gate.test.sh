#!/usr/bin/env bash
# Exercise scripts/release-ci-gate.sh offline: a throwaway git repository holds
# the commits and a stand-in origin/main, and a stub `gh` on PATH answers from
# canned GitHub JSON. The stub applies the gate's own --jq expressions with the
# real jq, so the filters the gate sends are what these cases exercise. Nothing
# here talks to GitHub or touches the real repository.
#
# Each case checks the exit code AND the reason, so a refusal for the wrong
# reason does not pass as the expected one.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$SCRIPT_DIR/release-ci-gate.sh"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/release-ci-gate-test.XXXXXX")"
trap 'rm -rf "$TMP_ROOT"' EXIT

command -v jq >/dev/null || { echo "release-ci-gate.test.sh: jq is required" >&2; exit 2; }

# A private HOME and no inherited GIT_CONFIG_* keep the developer's git config
# (signing, hooks) out of the fixture commits.
export HOME="$TMP_ROOT/home"
mkdir -p "$HOME"
unset GIT_CONFIG_COUNT GIT_CONFIG_PARAMETERS
for i in 0 1 2 3 4 5 6 7 8 9; do
  unset "GIT_CONFIG_KEY_$i" "GIT_CONFIG_VALUE_$i"
done
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com
export GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com

BIN_DIR="$TMP_ROOT/bin"
STATE="$TMP_ROOT/state"
mkdir -p "$BIN_DIR" "$STATE"

# Stub gh. `run list` answers from $STATE/runs.json, `run view <id>` from
# $STATE/jobs-<id>.json, and the branch-protection API from
# $STATE/protection.json (absent = HTTP 403, which is what the release
# workflow's token gets). Each answer goes through the caller's --jq like real
# gh does. Every call is logged to $STATE/gh-calls.
cat > "$BIN_DIR/gh" <<'STUB'
#!/usr/bin/env bash
set -u
STATE="${RELEASE_GATE_TEST_STATE:?}"
printf '%s\n' "$*" >> "$STATE/gh-calls"
jq_arg="."
prev=""
for arg in "$@"; do
  [ "$prev" = "--jq" ] && jq_arg="$arg"
  prev="$arg"
done
answer() {
  [ -f "$1" ] || { echo "gh stub: no fixture $1" >&2; exit 1; }
  jq -r "$jq_arg" "$1"
}
case "${1:-} ${2:-}" in
  "run list")
    [ -f "$STATE/fail-run-list" ] && { echo "HTTP 502" >&2; exit 1; }
    answer "$STATE/runs.json"
    ;;
  "run view") answer "$STATE/jobs-$3.json" ;;
  api\ *)
    case "$2" in
      */protection/required_status_checks)
        [ -f "$STATE/protection.json" ] || { echo "HTTP 403: Resource not accessible by integration" >&2; exit 1; }
        answer "$STATE/protection.json"
        ;;
      *) echo "gh stub: unhandled api $2" >&2; exit 1 ;;
    esac
    ;;
  *) echo "gh stub: unhandled: $*" >&2; exit 1 ;;
esac
STUB
chmod +x "$BIN_DIR/gh"

# Fixture repository: base <- landed (origin/main) and base <- stray, where
# stray never reached main.
REPO_DIR="$TMP_ROOT/repo"
git init -q -b main "$REPO_DIR"
mkdir -p "$REPO_DIR/.github"
cat > "$REPO_DIR/.github/required-checks.txt" <<'LIST'
# Fixture list; comments and blank lines are ignored.
Unit / Unit tests (Linux)

E2E / E2E (Linux Docker)
Unit / Bash permission e2e (Windows)
LIST
git -C "$REPO_DIR" add .github/required-checks.txt
git -C "$REPO_DIR" commit -q -m base
git -C "$REPO_DIR" commit -q --allow-empty -m landed
LANDED="$(git -C "$REPO_DIR" rev-parse HEAD)"
git -C "$REPO_DIR" update-ref refs/remotes/origin/main "$LANDED"
git -C "$REPO_DIR" checkout -q -b side HEAD~1
git -C "$REPO_DIR" commit -q --allow-empty -m stray
STRAY="$(git -C "$REPO_DIR" rev-parse HEAD)"
git -C "$REPO_DIR" checkout -q main
OTHER_SHA="0123456789abcdef0123456789abcdef01234567"

checks=0
failures=0
LAST_OUT=""
LAST_RC=0

ok() { checks=$((checks + 1)); printf 'release-ci-gate.test.sh: ok - %s\n' "$1"; }
fail() {
  printf 'release-ci-gate.test.sh: FAIL - %s\n' "$1" >&2
  printf '%s\n' "$LAST_OUT" | sed 's/^/    | /' >&2
  failures=$((failures + 1))
}

reset_state() {
  rm -rf "$STATE"
  mkdir -p "$STATE"
  : > "$STATE/gh-calls"
}

# run <url-id> <sha> <branch> <event> <status> <conclusion> <attempt>
run_json() {
  printf '{"databaseId":%s,"headSha":"%s","headBranch":"%s","event":"%s","status":"%s","conclusion":"%s","attempt":%s,"url":"https://github.com/example/repo/actions/runs/%s"}' \
    "$1" "$2" "$3" "$4" "$5" "$6" "$7" "$1"
}

write_runs() { printf '[%s]\n' "$(IFS=,; printf '%s' "$*")" > "$STATE/runs.json"; }

# write_jobs <run-id> "name=conclusion" ...
write_jobs() {
  local id="$1"
  shift
  local items=() pair
  for pair in "$@"; do
    items+=("$(jq -cn --arg n "${pair%=*}" --arg c "${pair##*=}" '{name: $n, status: "completed", conclusion: $c}')")
  done
  printf '{"jobs":[%s]}\n' "$(IFS=,; printf '%s' "${items[*]}")" > "$STATE/jobs-$id.json"
}

# Every job a fixture run normally carries: the three listed required checks,
# a blocking job that is not required, and a continue-on-error job.
all_green_jobs() {
  write_jobs "$1" \
    "Unit / Unit tests (Linux)=success" \
    "E2E / E2E (Linux Docker)=success" \
    "Unit / Bash permission e2e (Windows)=success" \
    "Unit / Plugin e2e (pi, Linux)=success" \
    "Search quality binary (Linux x64)=success"
}

gate() {
  set +e
  LAST_OUT="$(cd "$REPO_DIR" && PATH="$BIN_DIR:$PATH" RELEASE_GATE_TEST_STATE="$STATE" \
    GITHUB_REPOSITORY=example/repo GITHUB_ACTIONS='' "$GATE" "$@" 2>&1)"
  LAST_RC=$?
  set -e
}

# expect <rc> <needle> <case name>
expect() {
  if [ "$LAST_RC" -ne "$1" ]; then
    fail "$3: expected exit $1, got $LAST_RC"
  elif [[ "$LAST_OUT" != *"$2"* ]]; then
    fail "$3: output did not mention '$2'"
  else
    ok "$3"
  fi
}

# --- green run --------------------------------------------------------------
reset_state
write_runs "$(run_json 101 "$LANDED" train/release-1.0.0 push completed success 1)"
all_green_jobs 101
gate "$LANDED"
expect 0 "OK: https://github.com/example/repo/actions/runs/101 (branch train/release-1.0.0, attempt 1) is green with all 3 required checks success" \
  "green train run is releasable"
expect 0 "required checks: 3 from .github/required-checks.txt" "unreadable protection falls back to the committed list"

# An annotated tag resolves to the commit it points at.
git -C "$REPO_DIR" -c tag.gpgsign=false tag -a v1.0.0 -m release "$LANDED"
gate v1.0.0
expect 0 "commit $LANDED (v1.0.0)" "annotated tag is peeled to its commit"

# --- red run ----------------------------------------------------------------
# Every required job passed, but a blocking job that is not required failed,
# so the run concluded failure. Only the run conclusion can refuse this.
reset_state
write_runs "$(run_json 102 "$LANDED" train/release-1.0.0 push completed failure 1)"
write_jobs 102 \
  "Unit / Unit tests (Linux)=success" \
  "E2E / E2E (Linux Docker)=success" \
  "Unit / Bash permission e2e (Windows)=success" \
  "Unit / Plugin e2e (pi, Linux)=failure"
gate "$LANDED"
expect 1 "no green tests.yml run for $LANDED: https://github.com/example/repo/actions/runs/102 on train/release-1.0.0 is completed/failure" \
  "red run is refused"

# --- rerun green ------------------------------------------------------------
# GitHub reports the latest attempt; attempt 2 of a once-red run is green.
reset_state
write_runs "$(run_json 103 "$LANDED" train/release-1.0.0 push completed success 2)"
all_green_jobs 103
gate "$LANDED"
expect 0 "runs/103 (branch train/release-1.0.0, attempt 2) is green" "run rerun to green is releasable"

# A green train run is enough even when the later main run of the same commit
# was cancelled (what v0.59.0's commit carries).
reset_state
write_runs "$(run_json 105 "$LANDED" main push completed cancelled 1)" \
  "$(run_json 104 "$LANDED" train/release-1.0.0 push completed success 1)"
all_green_jobs 104
gate "$LANDED"
expect 0 "runs/104 (branch train/release-1.0.0, attempt 1) is green" "green train run outweighs a cancelled main run"

# --- wrong sha --------------------------------------------------------------
# A response carrying another commit's green run must not count, even if the
# server ignored the commit filter.
reset_state
write_runs "$(run_json 106 "$OTHER_SHA" train/other push completed success 1)"
all_green_jobs 106
gate "$LANDED"
expect 1 "no tests.yml run for $LANDED was started by a push to main or train/*" "another commit's green run is refused"

# --- missing run ------------------------------------------------------------
reset_state
write_runs
gate "$LANDED"
expect 1 "no tests.yml run for $LANDED was started by a push to main or train/*" "commit without any run is refused"

# Runs from other triggers or branches do not count.
reset_state
write_runs "$(run_json 107 "$LANDED" main workflow_dispatch completed success 1)" \
  "$(run_json 108 "$LANDED" feature/x push completed success 1)"
all_green_jobs 107
all_green_jobs 108
gate "$LANDED"
expect 1 "no tests.yml run for $LANDED was started by a push to main or train/*" "dispatch and non-train branch runs are refused"

# A run still in progress is not green.
reset_state
write_runs "$(run_json 109 "$LANDED" train/release-1.0.0 push in_progress "" 1)"
gate "$LANDED"
expect 1 "runs/109 on train/release-1.0.0 is in_progress/none" "unfinished run is refused"

# --- cancelled required job -------------------------------------------------
# The run concluded success, but a required job did not succeed (a
# continue-on-error job reports its own conclusion inside a green run).
reset_state
write_runs "$(run_json 110 "$LANDED" train/release-1.0.0 push completed success 1)"
write_jobs 110 \
  "Unit / Unit tests (Linux)=success" \
  "E2E / E2E (Linux Docker)=cancelled" \
  "Unit / Bash permission e2e (Windows)=failure"
gate "$LANDED"
expect 1 "runs/110 (branch train/release-1.0.0, attempt 1): 'E2E / E2E (Linux Docker)' cancelled; 'Unit / Bash permission e2e (Windows)' failure" \
  "cancelled and failed required jobs are refused by name"

# A required check that is not a job of the run is refused too.
reset_state
write_runs "$(run_json 111 "$LANDED" train/release-1.0.0 push completed success 1)"
write_jobs 111 \
  "Unit / Unit tests (Linux)=success" \
  "Unit / Bash permission e2e (Windows)=success"
gate "$LANDED"
expect 1 "'E2E / E2E (Linux Docker)' missing from the run" "required job absent from the run is refused"

# --- sha not on main --------------------------------------------------------
reset_state
write_runs "$(run_json 112 "$STRAY" train/stray push completed success 1)"
all_green_jobs 112
gate "$STRAY"
expect 1 "commit $STRAY is not on main" "commit not on main is refused"
if [ -s "$STATE/gh-calls" ]; then
  fail "commit not on main: gh was queried before the ancestry refusal"
else
  ok "commit not on main is refused before any gh query"
fi

# --- required set sources ---------------------------------------------------
# Readable protection wins over the list: here it requires a job the list
# does not name, and that job failed.
reset_state
write_runs "$(run_json 113 "$LANDED" train/release-1.0.0 push completed success 1)"
write_jobs 113 \
  "Unit / Unit tests (Linux)=success" \
  "E2E / E2E (Linux Docker)=success" \
  "Unit / Bash permission e2e (Windows)=success" \
  "Unit / Release storm (Linux)=failure"
printf '{"contexts":["Unit / Unit tests (Linux)","E2E / E2E (Linux Docker)","Unit / Bash permission e2e (Windows)","Unit / Release storm (Linux)"]}\n' \
  > "$STATE/protection.json"
gate "$LANDED"
expect 1 "'Unit / Release storm (Linux)' failure" "readable branch protection supplies the required set"
expect 1 "warning: .github/required-checks.txt differs from branch protection" "drift between protection and the committed list is reported"
expect 1 "release-ci-gate:   < Unit / Release storm (Linux)" "the drift report names the differing check"

# No readable protection and no list: fail closed.
reset_state
write_runs "$(run_json 114 "$LANDED" train/release-1.0.0 push completed success 1)"
all_green_jobs 114
mv "$REPO_DIR/.github/required-checks.txt" "$TMP_ROOT/required-checks.txt"
gate "$LANDED"
mv "$TMP_ROOT/required-checks.txt" "$REPO_DIR/.github/required-checks.txt"
expect 1 "no required checks known" "missing required set fails closed"

# A failing runs query fails closed.
reset_state
touch "$STATE/fail-run-list"
gate "$LANDED"
expect 1 "could not list tests.yml runs for $LANDED" "unreadable run list fails closed"

if [ "$failures" -ne 0 ]; then
  printf 'release-ci-gate.test.sh: %s check(s) failed\n' "$failures" >&2
  exit 1
fi
echo "release-ci-gate.test.sh: passed ($checks checks)"
