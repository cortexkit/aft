#!/usr/bin/env bash
# Exercise scripts/train-push.sh end to end against throwaway repositories: a
# local bare repo stands in for origin and a stub `gh` on PATH answers the
# queries watch-ci.sh makes. Nothing here talks to GitHub or touches the real
# repository.
#
# Covered: each precondition refusal, the dead-gate scan, the first-run trigger
# probe, a failing smoke (including a pipeline string whose failure hides behind
# a successful last stage), a red CI run, a clean land, and the re-queue - the
# default branch advances while CI is watching and the train rebases and lands
# on round 2.
#
# The fixtures' default branch is deliberately NOT called main: every fixture
# origin defaults to `master`, so any place the script assumes the name instead
# of reading it from the remote fails these rows.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TRAIN_PUSH="$SCRIPT_DIR/train-push.sh"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/train-push-test.XXXXXX")"
trap 'rm -rf "$TMP_ROOT"' EXIT

# Deliberately not "main": the fixtures' default branch is only discoverable by
# reading it from the remote, so any place the script assumes the name instead
# fails these rows.
DEFAULT_BRANCH="master"

# A private HOME keeps the developer's global git config (hooks, signing,
# aliases) out of the fixtures: a signing requirement in ~/.gitconfig would
# otherwise fail every commit here for a reason that has nothing to do with the
# script under test.
export HOME="$TMP_ROOT/home"
mkdir -p "$HOME"

# AFT runs agent shells with core.hooksPath injected through GIT_CONFIG_* env
# vars, which outrank repo config. Clearing them keeps the hook rows testing
# what the fixture configures rather than what this shell inherited.
unset GIT_CONFIG_COUNT GIT_CONFIG_PARAMETERS
for i in 0 1 2 3 4 5 6 7 8 9; do
  unset "GIT_CONFIG_KEY_$i" "GIT_CONFIG_VALUE_$i"
done

BIN_DIR="$TMP_ROOT/bin"
mkdir -p "$BIN_DIR"

# The governed-docs preflight shells out to bun and node. Those checks are not
# what this test is about, and the fixtures have no docs/ manifests, so both are
# stubbed green.
printf '#!/usr/bin/env bash\nexit 0\n' > "$BIN_DIR/bun"
printf '#!/usr/bin/env bash\nexit 0\n' > "$BIN_DIR/node"
chmod +x "$BIN_DIR/bun" "$BIN_DIR/node"

# Canned gh. It answers with the value real gh would print AFTER applying the
# --jq expression, selected by the --json field list, so watch-ci.sh's parsing
# is exercised unchanged. `on-watch.sh`, if present, runs once on the first
# query of a run: that is the window between the train branch push and the
# fast-forward attempt, which is where main has to move for the re-queue case.
cat > "$BIN_DIR/gh" <<'STUB'
#!/usr/bin/env bash
set -u
STATE="${TRAIN_PUSH_TEST_STATE:?gh stub needs TRAIN_PUSH_TEST_STATE}"

hook="$STATE/on-watch.sh"
if [ -x "$hook" ]; then
  mv "$hook" "$STATE/on-watch.running"
  if ! "$STATE/on-watch.running" >>"$STATE/on-watch.log" 2>&1; then
    # Loud on purpose: a hook that dies quietly turns the case it sets up into
    # a case that passes without ever being exercised.
    echo "gh stub: on-watch hook failed" >&2
    sed 's/^/gh stub: hook: /' "$STATE/on-watch.log" >&2
    exit 1
  fi
fi

json=""
jq_arg=""
prev=""
log_failed=0
for arg in "$@"; do
  [ "$prev" = "--json" ] && json="$arg"
  [ "$prev" = "--jq" ] && jq_arg="$arg"
  [ "$arg" = "--log-failed" ] && log_failed=1
  prev="$arg"
done

if [ "$log_failed" -eq 1 ]; then
  echo "FAIL [   0.10s] canned::stubbed_failure"
  exit 0
fi

case "$json" in
  defaultBranchRef) cat "$STATE/default_branch" ;;
  databaseId,headSha)
    # A green_shas file makes the stub sha-aware: shas listed there have a run,
    # any other sha has none yet (what a just-pushed commit looks like).
    if [ -f "$STATE/green_shas" ]; then
      sha="$(printf '%s' "$jq_arg" | sed -n 's/.*headSha=="\([0-9a-fA-F]*\)".*/\1/p')"
      grep -qx "$sha" "$STATE/green_shas" || exit 0
    fi
    cat "$STATE/run_id"
    ;;
  url) echo "https://github.com/cortexkit/aft/actions/runs/$(cat "$STATE/run_id")" ;;
  status) echo "completed" ;;
  jobs) cat "$STATE/failed_job" ;;
  conclusion) cat "$STATE/conclusion" ;;
  *)
    echo "gh stub: unhandled query: $*" >&2
    exit 1
    ;;
esac
STUB
chmod +x "$BIN_DIR/gh"

# Write a tests.yml whose on.push.branches list is the given lines.
write_tests_workflow() {
  local work="$1"
  local branches="$2"
  mkdir -p "$work/.github/workflows"
  {
    printf 'name: Tests\n'
    printf 'on:\n'
    printf '  pull_request:\n'
    printf '    paths:\n'
    printf '      - "crates/**"\n'
    printf '  push:\n'
    printf '    branches:\n'
    printf '%b\n' "$branches"
    printf 'jobs:\n'
    printf '  unit:\n'
    printf '    runs-on: ubuntu-latest\n'
    printf '    steps:\n'
    printf '      - run: "true"\n'
  } > "$work/.github/workflows/tests.yml"
}

failures=0
fail() {
  printf 'train-push.test.sh: FAIL — %s\n' "$1" >&2
  if [ -n "${LAST_OUT:-}" ]; then
    printf '%s\n' "$LAST_OUT" | sed 's/^/    | /' >&2
  fi
  failures=$((failures + 1))
}
ok() { printf 'train-push.test.sh: ok — %s\n' "$1"; }

# Build a fixture: a bare repo playing origin, a work clone holding the train,
# and a second clone that stands in for whoever else pushes to main.
new_fixture() {
  local name="$1"
  local dir="$TMP_ROOT/$name"
  mkdir -p "$dir"

  git init -q --bare "$dir/origin.git"
  git -C "$dir/origin.git" symbolic-ref HEAD "refs/heads/$DEFAULT_BRANCH"
  git init -q "$dir/work"
  git -C "$dir/work" symbolic-ref HEAD "refs/heads/$DEFAULT_BRANCH"
  git -C "$dir/work" config user.email "train@example.invalid"
  git -C "$dir/work" config user.name "Train Test"
  git -C "$dir/work" config commit.gpgsign false
  echo "base" > "$dir/work/base.txt"
  # The script refuses to push a train the workflow would not run on, so the
  # fixture carries a tests.yml shaped like the real one (a pull_request block
  # with its own lists above the push block the check has to read).
  write_tests_workflow "$dir/work" "      - $DEFAULT_BRANCH\n      - \"train/**\""
  git -C "$dir/work" add base.txt .github/workflows/tests.yml
  git -C "$dir/work" commit -qm "base"
  git -C "$dir/work" remote add origin "$dir/origin.git"
  git -C "$dir/work" push -q origin "$DEFAULT_BRANCH"
  git -C "$dir/work" fetch -q origin
  git -C "$dir/work" remote set-head origin -a >/dev/null

  mkdir -p "$dir/ci-state"
  echo "4242" > "$dir/ci-state/run_id"
  echo "success" > "$dir/ci-state/conclusion"
  echo "$DEFAULT_BRANCH" > "$dir/ci-state/default_branch"
  : > "$dir/ci-state/failed_job"

  # The first-run trigger probe has its own rows; every other fixture starts
  # already proven so it does not pay for a probe it is not testing.
  echo "pre-proven by the test harness" > "$dir/work/.git/train-push-proven"

  printf '%s\n' "$dir"
}

# A commit that only the train has, so a land is visible in origin by content.
add_train_commit() {
  local work="$1"
  local marker="$2"
  echo "$marker" > "$work/train.txt"
  git -C "$work" add train.txt
  git -C "$work" commit -qm "train: $marker"
}

# Push an unrelated commit onto origin/main, the way a peer landing a train
# during our CI run would. Pass a file the train also touches to make the
# rebase conflict instead.
advance_origin_main() {
  local dir="$1"
  local marker="$2"
  local file="${3:-other.txt}"
  rm -rf "$dir/peer"
  git clone -q "$dir/origin.git" "$dir/peer"
  git -C "$dir/peer" config user.email "peer@example.invalid"
  git -C "$dir/peer" config user.name "Peer"
  git -C "$dir/peer" config commit.gpgsign false
  echo "$marker" > "$dir/peer/$file"
  git -C "$dir/peer" add "$file"
  git -C "$dir/peer" commit -qm "peer: $marker"
  git -C "$dir/peer" push -q origin "HEAD:$DEFAULT_BRANCH"
}

LAST_OUT=""
LAST_RC=0
run_train() {
  local dir="$1"
  shift
  set +e
  LAST_OUT="$(
    cd "$dir/work" &&
      PATH="$BIN_DIR:$PATH" \
      OPERATOR_GH_FALLBACK_PATHS="$TMP_ROOT/no-such-fallback" \
      TRAIN_PUSH_TEST_STATE="$dir/ci-state" \
      WATCH_CI_RESOLVE_ATTEMPTS=1 \
      WATCH_CI_RESOLVE_SLEEP=0 \
      TRAIN_PUSH_PROBE_ATTEMPTS=2 \
      TRAIN_PUSH_PROBE_SLEEP=0 \
      "$TRAIN_PUSH" "$@" 2>&1
  )"
  LAST_RC=$?
  set -e
}

# Never returns nonzero: these are called bare, and under `set -e` a failing
# check would kill the run at the first bad row instead of reporting the rest.
expect_rc() {
  local want="$1"
  local what="$2"
  if [ "$LAST_RC" -eq "$want" ]; then
    ok "$what (exit $want)"
  else
    fail "$what: expected exit $want, got $LAST_RC"
  fi
  return 0
}

expect_out() {
  local needle="$1"
  local what="$2"
  case "$LAST_OUT" in
    *"$needle"*) ok "$what" ;;
    *) fail "$what: output did not mention '$needle'" ;;
  esac
}

expect_no_out() {
  local needle="$1"
  local what="$2"
  case "$LAST_OUT" in
    *"$needle"*) fail "$what: output mentioned '$needle'" ;;
    *) ok "$what" ;;
  esac
}

# A second workflow carrying one gated job (and optionally one gated step), so
# the dead-gate scan has something to find beside a normal trigger list.
write_gated_workflow() {
  local work="$1"
  local job_if="$2"
  local step_if="$3"
  mkdir -p "$work/.github/workflows"
  {
    printf 'name: Extra\n'
    printf 'on:\n'
    printf '  push:\n'
    printf '    branches:\n'
    printf '      - %s\n' "$DEFAULT_BRANCH"
    printf '      - "train/**"\n'
    printf 'jobs:\n'
    printf '  gated:\n'
    printf '    runs-on: ubuntu-latest\n'
    [ -n "$job_if" ] && printf '    if: %s\n' "$job_if"
    printf '    steps:\n'
    printf '      - name: publish\n'
    [ -n "$step_if" ] && printf '        if: %s\n' "$step_if"
    printf '        run: "true"\n'
  } > "$work/.github/workflows/extra.yml"
}

origin_ref() { git -C "$1/origin.git" rev-parse --verify -q "$2" || true; }

# --- refusal: no train name ------------------------------------------------
dir="$(new_fixture usage)"
run_train "$dir"
expect_rc 2 "no train name refuses"
expect_out "no train name given" "no train name names the reason"

# --- refusal: unusable train name ------------------------------------------
dir="$(new_fixture badname)"
run_train "$dir" "bad name"
expect_rc 2 "unusable train name refuses"

# --- refusal: tests.yml would not run on the train branch ------------------
dir="$(new_fixture notrigger)"
write_tests_workflow "$dir/work" "      - $DEFAULT_BRANCH"
git -C "$dir/work" add .github/workflows/tests.yml
git -C "$dir/work" commit -qm "narrow trigger"
run_train "$dir" notrigger
expect_rc 2 "a workflow that skips train branches refuses"
expect_out "add \`train/**\` to on.push.branches" "narrow trigger prints the one-line fix"
[ -z "$(origin_ref "$dir" refs/heads/train/notrigger)" ] ||
  fail "narrow trigger refusal still pushed a train branch"

# --- refusal: no tests.yml at all ------------------------------------------
dir="$(new_fixture noworkflow)"
git -C "$dir/work" rm -q .github/workflows/tests.yml
git -C "$dir/work" commit -qm "drop workflow"
run_train "$dir" noworkflow
expect_rc 2 "a missing tests.yml refuses"
expect_out "add \`train/**\` to on.push.branches" "missing workflow prints the one-line fix"

# --- a catch-all trigger passes the check (it falls to the next one) --------
dir="$(new_fixture widetrigger)"
write_tests_workflow "$dir/work" '      - "**"'
git -C "$dir/work" add .github/workflows/tests.yml
git -C "$dir/work" commit -qm "catch-all trigger"
echo "uncommitted" > "$dir/work/base.txt"
run_train "$dir" widetrigger
expect_rc 2 "catch-all trigger reaches the later preconditions"
expect_out "working tree is not clean" "catch-all trigger is accepted by the trigger check"

# --- refusal: a job gated on the default branch's ref ----------------------
dir="$(new_fixture refgate)"
write_gated_workflow "$dir/work" "github.ref == 'refs/heads/$DEFAULT_BRANCH'" ""
git -C "$dir/work" add .github/workflows/extra.yml
git -C "$dir/work" commit -qm "ref-gated job"
run_train "$dir" refgate
expect_rc 2 "a ref-gated job refuses"
expect_out ".github/workflows/extra.yml [on: push (branches: $DEFAULT_BRANCH, train/**)]" \
  "ref gate is printed under its file's trigger list"
expect_out "job gated: github.ref == 'refs/heads/$DEFAULT_BRANCH'" \
  "ref gate names the job and the condition"
expect_out "widen the ref condition to include refs/heads/train/" "ref gate prints the fix"
[ -z "$(origin_ref "$dir" refs/heads/train/refgate)" ] ||
  fail "ref-gate refusal still pushed a train branch"

# --- warning only: a step gated on the event name --------------------------
# Whether that gate is dead depends on how the repo lands changes, which the
# scanner cannot see, so it says so and gets out of the way.
dir="$(new_fixture eventgate)"
write_gated_workflow "$dir/work" "" "github.event_name == 'pull_request'"
git -C "$dir/work" add .github/workflows/extra.yml
git -C "$dir/work" commit -qm "event-gated step"
event_sha="$(git -C "$dir/work" rev-parse HEAD)"
run_train "$dir" eventgate
expect_rc 0 "an event-gated step warns but does not refuse"
expect_out "WARNING" "event gate warns"
expect_out "job gated, step publish: github.event_name == 'pull_request'" \
  "event gate names the job, the step and the condition"
expect_out "judge this against how the repo lands; the scanner cannot know the landing path" \
  "event gate prints the judgement sentence"
[ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$event_sha" ] ||
  fail "event-gate warning blocked the land"

# --- widened condition: nothing to say -------------------------------------
dir="$(new_fixture widened)"
write_gated_workflow "$dir/work" \
  "github.ref == 'refs/heads/$DEFAULT_BRANCH' || startsWith(github.ref, 'refs/heads/train/')" ""
git -C "$dir/work" add .github/workflows/extra.yml
git -C "$dir/work" commit -qm "widened gate"
run_train "$dir" widened
expect_rc 0 "a widened ref condition passes"
expect_no_out "WARNING" "widened condition produces no warning"
expect_no_out "refusing" "widened condition produces no refusal"

# --- a folded `if:` is still a ref gate ------------------------------------
# The condition is on the lines after `if: >-`, where a line-oriented search
# finds nothing. The branches list is in flow style for the same reason.
dir="$(new_fixture folded)"
add_train_commit "$dir/work" "folded"
cat > "$dir/work/.github/workflows/extra.yml" <<YAML
name: Extra
on:
  push:
    branches: [$DEFAULT_BRANCH, "train/**"]
concurrency:
  group: extra-fixed-group
  cancel-in-progress: true
jobs:
  gated:
    runs-on: ubuntu-latest
    if: >-
      github.ref == 'refs/heads/$DEFAULT_BRANCH' &&
      github.event_name == 'push'
    steps:
      - run: "true"
YAML
git -C "$dir/work" add .github/workflows/extra.yml
git -C "$dir/work" commit -qm "folded ref gate"
run_train "$dir" folded
expect_rc 2 "a folded ref condition is still found"
expect_out "job gated: github.ref == 'refs/heads/$DEFAULT_BRANCH' && github.event_name == 'push'" \
  "the folded condition is read as one whole string"
expect_out "[concurrency: extra-fixed-group]" "the header carries the concurrency group"

# --- a concurrency group that is not per-ref warns -------------------------
# Such a group makes a train push cancel an in-flight run on another ref, which
# is worth saying out loud - and is not a reason to refuse the train.
dir="$(new_fixture concurrency)"
add_train_commit "$dir/work" "concurrency"
cat > "$dir/work/.github/workflows/extra.yml" <<YAML
name: Extra
on:
  push:
    branches:
      - $DEFAULT_BRANCH
      - "train/**"
concurrency:
  group: one-group-for-every-ref
  cancel-in-progress: true
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: "true"
YAML
git -C "$dir/work" add .github/workflows/extra.yml
git -C "$dir/work" commit -qm "shared concurrency group"
run_train "$dir" concurrency
expect_rc 0 "a shared concurrency group warns but does not refuse"
expect_out "concurrency group is not per-ref: one-group-for-every-ref" \
  "the shared group is named"

# --- first-run trigger probe -----------------------------------------------
dir="$(new_fixture probe)"
rm -f "$dir/work/.git/train-push-proven"
add_train_commit "$dir/work" "probe"
run_train "$dir" probe
expect_rc 0 "the first train probes the trigger and lands"
expect_out "first train in this repo" "first run announces the probe"
expect_out "trigger proven by run 4242" "first run records the run that proved it"
grep -q "run 4242" "$dir/work/.git/train-push-proven" ||
  fail "probe marker does not carry the run id"
[ -z "$(origin_ref "$dir" refs/heads/train/trigger-probe)" ] ||
  fail "probe left its branch behind on origin"
add_train_commit "$dir/work" "probe-second"
run_train "$dir" probe
expect_rc 0 "the second train lands too"
expect_no_out "first train in this repo" "second run skips the probe"

# --- warning: a repo-local pre-push hook -----------------------------------
# Such a hook would re-run the gate on every fix-and-repush and on the branch
# deletion after a land, so it is reported - but never run, and never a refusal.
dir="$(new_fixture prepush)"
add_train_commit "$dir/work" "prepush"
printf '#!/bin/sh\nexit 0\n' > "$dir/work/.git/hooks/pre-push"
chmod +x "$dir/work/.git/hooks/pre-push"
run_train "$dir" prepush
expect_rc 0 "a pre-push hook warns but does not refuse"
# Suffix, not the whole path: git normalises the temp dir it is given.
expect_out "repo-local pre-push hook: " "pre-push warning names the hook"
expect_out "prepush/work/.git/hooks/pre-push" "pre-push warning gives the full path"
expect_out "verify it does not refuse pushes to train/** or ref deletions" \
  "pre-push warning says what to verify"

# --- no warning: AFT's managed dispatcher is not itself a gate -------------
dir="$(new_fixture dispatcher)"
add_train_commit "$dir/work" "dispatcher"
# Recognised by the shape of the path, so the fixture keeps it inside the
# throwaway tree instead of writing into a real AFT data directory.
managed_hooks="$TMP_ROOT/fake-data/cortexkit/aft/git-hooks"
mkdir -p "$managed_hooks"
printf '#!/bin/sh\nexit 0\n' > "$managed_hooks/pre-push"
chmod +x "$managed_hooks/pre-push"
git -C "$dir/work" config core.hooksPath "$managed_hooks"
run_train "$dir" dispatcher
expect_rc 0 "the managed dispatcher does not block a train"
expect_no_out "pre-push hook" "the managed dispatcher alone is not warned about"

# --- red that is dependency skew: named, and never re-queued ---------------
dir="$(new_fixture skew)"
add_train_commit "$dir/work" "skew"
skew_sha="$(git -C "$dir/work" rev-parse HEAD)"
echo "Check sibling locks|9002" > "$dir/ci-state/failed_job"
echo "failure" > "$dir/ci-state/conclusion"
# Would move the default branch if anything re-queued: a rebase must not happen
# on a red run at all.
cat > "$dir/ci-state/on-watch.sh" <<HOOK
#!/usr/bin/env bash
set -euo pipefail
export HOME="$HOME"
DEFAULT_BRANCH="$DEFAULT_BRANCH"
$(declare -f advance_origin_main)
advance_origin_main "$dir" "raced-during-skew"
HOOK
chmod +x "$dir/ci-state/on-watch.sh"
run_train "$dir" skew
expect_rc 1 "a lock-skew red exits 1"
expect_out "red is a version/lock skew, not contention: this terminates in a lockfile bump commit, not a retry" \
  "lock-skew red says where it terminates"
expect_out "Check sibling locks" "lock-skew red still names the job"
expect_no_out "rebased onto" "lock-skew red never rebases"
expect_no_out "(round 2 of 3)" "lock-skew red never starts a second round"
[ "$(git -C "$dir/work" rev-parse HEAD)" = "$skew_sha" ] ||
  fail "lock-skew red moved HEAD, so something rebased"

# --- refusal: dirty tree ---------------------------------------------------
dir="$(new_fixture dirty)"
echo "uncommitted" > "$dir/work/base.txt"
run_train "$dir" dirty
expect_rc 2 "dirty tree refuses"
expect_out "working tree is not clean" "dirty tree names the reason"
[ -z "$(origin_ref "$dir" refs/heads/train/dirty)" ] ||
  fail "dirty tree refusal still pushed a train branch"

# --- refusal: unresolved git operation -------------------------------------
dir="$(new_fixture midop)"
git -C "$dir/work" rev-parse HEAD > "$dir/work/.git/MERGE_HEAD"
run_train "$dir" midop
expect_rc 2 "MERGE_HEAD refuses"
expect_out "MERGE_HEAD present" "MERGE_HEAD names the reason"

# --- refusal: local main behind origin/main --------------------------------
dir="$(new_fixture behind)"
advance_origin_main "$dir" "landed-elsewhere"
run_train "$dir" behind
expect_rc 2 "a local default branch behind the remote refuses"
expect_out "is behind origin/$DEFAULT_BRANCH" "behind the default branch names the reason"

# --- refusal: smoke command fails ------------------------------------------
dir="$(new_fixture smoke)"
add_train_commit "$dir/work" "smoke"
run_train "$dir" smoke -- false
expect_rc 2 "failing smoke refuses"
expect_out "local smoke failed" "failing smoke names the reason"
[ -z "$(origin_ref "$dir" refs/heads/train/smoke)" ] ||
  fail "failing smoke still pushed a train branch"

# --- refusal: smoke pipeline whose failure is not in the last stage ---------
# Without pipefail this pipeline exits 0 and the train would sail past a broken
# slice, which is the whole reason the smoke runs under pipefail.
dir="$(new_fixture smokepipe)"
add_train_commit "$dir/work" "smokepipe"
run_train "$dir" smokepipe -- "false | cat"
expect_rc 2 "failing pipeline smoke refuses"

# --- CI red: branch stays, job named ---------------------------------------
dir="$(new_fixture red)"
add_train_commit "$dir/work" "red"
echo "Unit (ubuntu-latest)|9001" > "$dir/ci-state/failed_job"
echo "failure" > "$dir/ci-state/conclusion"
run_train "$dir" red
expect_rc 1 "red CI exits 1"
expect_out "Unit (ubuntu-latest)" "red CI names the failing job"
expect_out "actions/runs/4242" "red CI prints the run url"
[ -n "$(origin_ref "$dir" refs/heads/train/red)" ] ||
  fail "red CI deleted the train branch instead of leaving it to fix"
[ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$(git -C "$dir/work" rev-parse HEAD~1)" ] ||
  fail "red CI moved origin/main"

# --- green: lands on main, train branch cleaned up -------------------------
dir="$(new_fixture green)"
add_train_commit "$dir/work" "green"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
run_train "$dir" green
expect_rc 0 "green CI lands"
[ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$train_sha" ] ||
  fail "green CI did not fast-forward origin/main to the tested sha"
[ -z "$(origin_ref "$dir" refs/heads/train/green)" ] ||
  fail "green CI left the train branch behind"

# --- re-queue: main moves during CI, train rebases and lands on round 2 -----
dir="$(new_fixture requeue)"
add_train_commit "$dir/work" "requeue"
first_sha="$(git -C "$dir/work" rev-parse HEAD)"
# Fires from inside the stubbed gh, i.e. after the branch push and before the
# fast-forward attempt.
cat > "$dir/ci-state/on-watch.sh" <<HOOK
#!/usr/bin/env bash
set -euo pipefail
export HOME="$HOME"
DEFAULT_BRANCH="$DEFAULT_BRANCH"
$(declare -f advance_origin_main)
advance_origin_main "$dir" "raced"
HOOK
chmod +x "$dir/ci-state/on-watch.sh"
run_train "$dir" requeue
expect_rc 0 "moved main re-queues and lands"
expect_out "moved to" "re-queue reports the sha main moved to"
expect_out "(round 1 of 3)" "re-queue reports the round it lost"
expect_out "(round 2 of 3)" "re-queue watched a second run"
expect_out "rebased onto" "re-queue reports the rebase"
landed="$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
[ "$landed" = "$(git -C "$dir/work" rev-parse HEAD)" ] ||
  fail "re-queue landed something other than the rebased train head"
[ "$landed" != "$first_sha" ] ||
  fail "re-queue landed the pre-rebase sha, so no rebase happened"
git -C "$dir/origin.git" cat-file -e "$DEFAULT_BRANCH:train.txt" 2>/dev/null ||
  fail "re-queue lost the train's own commit"
git -C "$dir/origin.git" cat-file -e "$DEFAULT_BRANCH:other.txt" 2>/dev/null ||
  fail "re-queue dropped the commit that moved main"
[ "$(git -C "$dir/work" rev-list --count HEAD)" -eq 3 ] ||
  fail "re-queue did not produce a linear rebase onto the moved main"

# --- re-queue exhausted: main moves every round, exit 3 after the third ----
dir="$(new_fixture exhausted)"
add_train_commit "$dir/work" "exhausted"
# Re-arms itself, so main is ahead again on every round's fast-forward attempt.
cat > "$dir/ci-state/on-watch.sh" <<HOOK
#!/usr/bin/env bash
set -euo pipefail
export HOME="$HOME"
DEFAULT_BRANCH="$DEFAULT_BRANCH"
$(declare -f advance_origin_main)
advance_origin_main "$dir" "raced-\$\$"
cp "$dir/ci-state/on-watch.running" "$dir/ci-state/on-watch.sh"
chmod +x "$dir/ci-state/on-watch.sh"
HOOK
chmod +x "$dir/ci-state/on-watch.sh"
run_train "$dir" exhausted
expect_rc 3 "main moving every round exits 3"
expect_out "(round 3 of 3)" "exhausted re-queue used all three rounds"
expect_out "gave up after 3 rounds" "exhausted re-queue says why it stopped"
[ -n "$(origin_ref "$dir" refs/heads/train/exhausted)" ] ||
  fail "exhausted re-queue deleted the train branch"
rm -f "$dir/ci-state/on-watch.sh" "$dir/ci-state/on-watch.running"

# --- rebase conflict: abort, name the files, leave the branch as it was ----
dir="$(new_fixture conflict)"
echo "train edit" > "$dir/work/base.txt"
git -C "$dir/work" add base.txt
git -C "$dir/work" commit -qm "train: edit base"
conflict_sha="$(git -C "$dir/work" rev-parse HEAD)"
cat > "$dir/ci-state/on-watch.sh" <<HOOK
#!/usr/bin/env bash
set -euo pipefail
export HOME="$HOME"
DEFAULT_BRANCH="$DEFAULT_BRANCH"
$(declare -f advance_origin_main)
advance_origin_main "$dir" "peer edit" base.txt
HOOK
chmod +x "$dir/ci-state/on-watch.sh"
run_train "$dir" conflict
expect_rc 3 "rebase conflict exits 3"
expect_out "conflicted" "rebase conflict says the rebase conflicted"
expect_out "base.txt" "rebase conflict names the conflicting file"
[ "$(git -C "$dir/work" rev-parse HEAD)" = "$conflict_sha" ] ||
  fail "rebase conflict did not restore HEAD"
[ ! -e "$dir/work/.git/rebase-merge" ] && [ ! -e "$dir/work/.git/rebase-apply" ] ||
  fail "rebase conflict left the tree mid-rebase"
[ -z "$(git -C "$dir/work" status --porcelain)" ] ||
  fail "rebase conflict left the working tree dirty"
[ "$(origin_ref "$dir" refs/heads/train/conflict)" = "$conflict_sha" ] ||
  fail "rebase conflict moved the train branch"

# --- stale green never authorizes a land -----------------------------------
# Round 1 goes green, main moves, the train rebases - and the rebased sha has no
# run yet. The old sha's green must not be spent on the new one: the script has
# to wait for a check on what it actually rebased.
dir="$(new_fixture stalegreen)"
add_train_commit "$dir/work" "stalegreen"
old_sha="$(git -C "$dir/work" rev-parse HEAD)"
echo "$old_sha" > "$dir/ci-state/green_shas"
cat > "$dir/ci-state/on-watch.sh" <<HOOK
#!/usr/bin/env bash
set -euo pipefail
export HOME="$HOME"
DEFAULT_BRANCH="$DEFAULT_BRANCH"
$(declare -f advance_origin_main)
advance_origin_main "$dir" "raced-after-green"
HOOK
chmod +x "$dir/ci-state/on-watch.sh"
run_train "$dir" stalegreen
expect_rc 2 "a rebased sha with no run does not land on the old sha's green"
expect_out "(round 2 of 3)" "stale green forced a second round"
expect_out "no tests.yml run appeared" "stale green waited for the rebased sha's run"
rebased_sha="$(git -C "$dir/work" rev-parse HEAD)"
[ "$rebased_sha" != "$old_sha" ] || fail "stale green case never rebased"
[ "$(origin_ref "$dir" refs/heads/train/stalegreen)" = "$rebased_sha" ] ||
  fail "stale green did not re-push the rebased sha"
if git -C "$dir/origin.git" cat-file -e "$DEFAULT_BRANCH:train.txt" 2>/dev/null; then
  fail "stale green landed the train on the old sha's check"
fi
git -C "$dir/origin.git" cat-file -e "$DEFAULT_BRANCH:other.txt" 2>/dev/null ||
  fail "stale green fixture never moved origin/main"

# --- protection refuses an unchecked sha: say what to do about it -----------
dir="$(new_fixture protected)"
add_train_commit "$dir/work" "protected"
protected_sha="$(git -C "$dir/work" rev-parse HEAD)"
cat > "$dir/origin.git/hooks/pre-receive" <<HOOK
#!/bin/sh
# Stand in for GitHub branch protection: the default branch only accepts a sha
# that already carries a check.
while read -r _old _new ref; do
  if [ "\$ref" = "refs/heads/$DEFAULT_BRANCH" ]; then
    echo 'error: GH006: Protected branch update failed for refs/heads/$DEFAULT_BRANCH.' >&2
    echo 'error: Required status check Unit is expected.' >&2
    exit 1
  fi
done
exit 0
HOOK
chmod +x "$dir/origin.git/hooks/pre-receive"
run_train "$dir" protected
expect_rc 1 "a protection refusal exits 1"
expect_out "refused: $protected_sha has no status check on origin. Merge onto the train branch and push there; CI runs on the merge sha, then main fast-forwards." \
  "protection refusal prints the merge-as-a-train remedy"
[ -n "$(origin_ref "$dir" refs/heads/train/protected)" ] ||
  fail "protection refusal deleted the train branch"

# --- this repository's own workflows do not block trains -------------------
# The scan is only worth anything if it agrees with the workflows we actually
# ship: a ref gate added to any of them would refuse every train, and finding
# that out here beats finding it out mid-push.
real_workflows="$SCRIPT_DIR/../.github/workflows"
if [ -d "$real_workflows" ]; then
  dir="$(new_fixture realworkflows)"
  rm -f "$dir"/work/.github/workflows/*.yml
  cp "$real_workflows"/*.yml "$dir/work/.github/workflows/"
  git -C "$dir/work" add -A .github/workflows
  git -C "$dir/work" commit -qm "this repo's workflows"
  run_train "$dir" realworkflows
  expect_rc 0 "the repository's own workflows pass step 0"
  expect_no_out "refusing" "no shipped workflow blocks a train"
else
  fail "could not find $real_workflows to check the shipped workflows against"
fi

# --- lifted into a repository with no preflight scripts --------------------
# Other repositories carry this script as a file. A fixture has none of this
# repository's governed-docs scripts, so a train there must land with the header
# naming that nothing ran, rather than refusing at step zero over a file that
# does not exist (the first lift failed exactly that way).
dir="$(new_fixture lifted)"
add_train_commit "$dir/work" "lifted"
run_train "$dir" lifted
expect_rc 0 "a repository with no preflight scripts lands"
expect_out "preflights: none" "the header says no preflight ran"
expect_no_out "governed-surface" "no governed-docs gate ran where its script is absent"

# --- repo-local preflight hook: named when it runs, able to refuse ---------
dir="$(new_fixture localhook)"
mkdir -p "$dir/work/scripts"
printf '#!/usr/bin/env bash\n: > "%s/hook-ran"\n' "$dir" > "$dir/work/scripts/train-push.local.sh"
git -C "$dir/work" add scripts/train-push.local.sh
git -C "$dir/work" commit -qm "local preflight"
add_train_commit "$dir/work" "hooked"
run_train "$dir" hooked
expect_rc 0 "a passing local preflight lets the train land"
expect_out "preflights: train-push.local.sh" "the header names the local hook"
if [ -f "$dir/hook-ran" ]; then ok "the local hook actually ran"; else fail "the local hook was named but never ran"; fi

dir="$(new_fixture localrefuse)"
mkdir -p "$dir/work/scripts"
printf '#!/usr/bin/env bash\nrefuse "local preflight says no"\n' > "$dir/work/scripts/train-push.local.sh"
git -C "$dir/work" add scripts/train-push.local.sh
git -C "$dir/work" commit -qm "refusing local preflight"
add_train_commit "$dir/work" "refused"
run_train "$dir" refused
expect_rc 2 "a refusing local preflight stops the train before any push"
expect_out "local preflight says no" "the refusal reason is the hook's own"
if git -C "$dir/origin.git" show-ref --quiet "refs/heads/train/refused"; then
  fail "a refused preflight still pushed the train branch"
else
  ok "nothing was pushed after the preflight refused"
fi

# --- the governed-docs gate still runs where its script exists ------------
# The existence check must not turn into a skip: with the audit script present
# and failing, the train refuses. `bun` is the harness's exit-0 stub on PATH, so
# this fixture shadows it with a failing one for the run.
dir="$(new_fixture auditfails)"
mkdir -p "$dir/work/scripts" "$dir/failbin"
: > "$dir/work/scripts/audit-v049-agent-surface.ts"
git -C "$dir/work" add scripts/audit-v049-agent-surface.ts
git -C "$dir/work" commit -qm "carry the audit script"
add_train_commit "$dir/work" "audited"
printf '#!/usr/bin/env bash\nexit 1\n' > "$dir/failbin/bun"
chmod +x "$dir/failbin/bun"
saved_bin="$BIN_DIR"
BIN_DIR="$dir/failbin:$saved_bin"
run_train "$dir" audited
BIN_DIR="$saved_bin"
expect_rc 2 "a present, failing governed-surface audit refuses the train"
expect_out "governed-surface audit failed" "the refusal names the gate"

if [ "$failures" -ne 0 ]; then
  printf 'train-push.test.sh: %s check(s) failed\n' "$failures" >&2
  exit 1
fi
echo "train-push.test.sh: passed"
