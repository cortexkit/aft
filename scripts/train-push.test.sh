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
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
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
printf '%s\n' "$*" >> "$STATE/gh-calls"
if [ -d "$STATE/runs" ]; then
  exec python3 "$(dirname "$0")/stack-gh.py" "$@"
fi

# A rerun changes the attempt number without changing the run id. The state
# transition is controllable so recovery tests can prove they watched the new
# attempt rather than accepting the completed result that prompted the rerun.
if [ "${1:-}" = "run" ] && [ "${2:-}" = "rerun" ]; then
  printf 'bypass=%s %s\n' "${GH_SHIM_BYPASS:-}" "$*" >> "$STATE/reruns"
  attempt="$(cat "$STATE/attempt" 2>/dev/null || echo 1)"
  printf '%s\n' "$((attempt + 1))" > "$STATE/attempt"
  if [ -f "$STATE/rerun_conclusion" ]; then
    cp "$STATE/rerun_conclusion" "$STATE/conclusion"
  fi
  if [ "$(cat "$STATE/conclusion")" = "success" ]; then
    : > "$STATE/failed_job"
    : > "$STATE/rerun_jobs"
  fi
  echo completed > "$STATE/recorded_status"
  exit 0
fi

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

# Job log (`run view --job <id> --log`): the fixture's log_tail file stands in
# for the failing job's whole log; the script judges only its last lines.
job_id=""
for arg in "$@"; do
  [ "$prev" = "--job" ] && job_id="$arg"
  prev="$arg"
done
if [ -n "$job_id" ]; then
  cat "$STATE/log_tail" 2>/dev/null || true
  exit 0
fi

# The jobs API the skew check reads step names from.
if [ "${1:-}" = "api" ]; then
  cat "$STATE/job_steps" 2>/dev/null || true
  exit 0
fi

# Fault injection: a field listed in fail_json makes the combined `run view`
# response omit that field's usable value, standing in for a completed-run
# payload whose conclusion or job list could not be read.
if [ "${1:-}" = "run" ] && [ "${2:-}" = "view" ] && [ -f "$STATE/fail_json" ] &&
  grep -qxF "$json" "$STATE/fail_json"; then
  exit 1
fi

if [ "$json" = "status,conclusion,jobs,url" ]; then
  watch_status="$(cat "$STATE/watch_status" 2>/dev/null || echo completed)"
  printf '%s\n' "$watch_status" > "$STATE/recorded_status"
  if [ -f "$STATE/capture_heartbeat" ] && [ -n "${WATCH_CI_HEARTBEAT:-}" ] && [ -f "$WATCH_CI_HEARTBEAT" ]; then
    cp "$WATCH_CI_HEARTBEAT" "$STATE/heartbeat-snapshot"
  fi
  combined_conclusion="$(cat "$STATE/conclusion")"
  combined_jobs="$(cat "$STATE/failed_job" 2>/dev/null || true)"
  combined_count="$(cat "$STATE/job_count" 2>/dev/null || echo 3)"
  fail_conclusion=0
  fail_jobs=0
  if [ -f "$STATE/fail_json" ]; then
    grep -qxF conclusion "$STATE/fail_json" && fail_conclusion=1 || true
    grep -qxF jobs "$STATE/fail_json" && fail_jobs=1 || true
  fi
  python3 - "$watch_status" "$combined_conclusion" "$combined_jobs" "$combined_count" \
    "$fail_conclusion" "$fail_jobs" "$(cat "$STATE/run_id")" <<'PY'
import json, sys
status, conclusion, failed, count, fail_conclusion, fail_jobs, run_id = sys.argv[1:]
jobs = [
    {"name": f"Unit {index + 1}", "databaseId": 9100 + index, "conclusion": "success"}
    for index in range(int(count))
]
if failed:
    name, _, job_id = failed.partition("|")
    jobs.append({"name": name, "databaseId": int(job_id or 0), "conclusion": "failure"})
if fail_conclusion == "1":
    conclusion = None
if fail_jobs == "1":
    jobs = []
print(json.dumps({
    "status": status,
    "conclusion": conclusion,
    "jobs": jobs,
    "url": f"https://github.com/example/repo/actions/runs/{run_id}",
}))
PY
  exit 0
fi

case "$json" in
  defaultBranchRef) cat "$STATE/default_branch" ;;
  databaseId,headSha|databaseId,headSha,workflowName|databaseId,headSha,headBranch)
    # A green_shas file makes the stub sha-aware: shas listed there have a run,
    # any other sha has none yet (what a just-pushed commit looks like).
    if [ -f "$STATE/green_shas" ]; then
      sha="$(printf '%s' "$jq_arg" | sed -n 's/.*headSha=="\([0-9a-fA-F]*\)".*/\1/p')"
      grep -qx "$sha" "$STATE/green_shas" || exit 0
    fi
    if [ "$json" = "databaseId,headSha,headBranch" ]; then
      branch="$(printf '%s' "$jq_arg" | sed -n 's/.*headBranch=="\([^"]*\)".*/\1/p')"
      have_branch="$(cat "$STATE/run_branch" 2>/dev/null || true)"
      [ -n "$have_branch" ] || have_branch="${TRAIN_PUSH_TEST_BRANCH:-${WATCH_CI_BRANCH:-}}"
      [ -z "$branch" ] || [ "$branch" = "$have_branch" ] || exit 0
    fi
    # The probe matches its run by the workflow's display name. The stub's run
    # belongs to the workflow named in $STATE/workflow_name (default: the name
    # write_tests_workflow gives); a query for any other name finds no run,
    # which is what the forge answers when the names disagree.
    if [ "$json" = "databaseId,headSha,workflowName" ]; then
      want="$(printf '%s' "$jq_arg" | sed -n 's/.*workflowName=="\([^"]*\)".*/\1/p')"
      have="$(cat "$STATE/workflow_name" 2>/dev/null || echo Tests)"
      [ "$want" = "$have" ] || exit 0
    fi
    cat "$STATE/run_id"
    ;;
  url) echo "https://github.com/example/repo/actions/runs/$(cat "$STATE/run_id")" ;;
  status,conclusion)
    printf '%s\t%s\n' "$(cat "$STATE/recorded_status" 2>/dev/null || echo completed)" "$(cat "$STATE/conclusion")"
    ;;
  status)
    if [ -f "$STATE/capture_heartbeat" ] && [ -n "${WATCH_CI_HEARTBEAT:-}" ] && [ -f "$WATCH_CI_HEARTBEAT" ]; then
      cp "$WATCH_CI_HEARTBEAT" "$STATE/heartbeat-snapshot"
    fi
    watch_status="$(cat "$STATE/watch_status" 2>/dev/null || echo completed)"
    # Both queries describe one run. Once the watch sees completion, a later
    # status+conclusion lookup must not keep returning the old running status.
    printf '%s\n' "$watch_status" > "$STATE/recorded_status"
    printf '%s\n' "$watch_status"
    ;;
  jobs)
    case "$jq_arg" in
      *'.conclusion + "|"'*) cat "$STATE/rerun_jobs" ;;
      *'| length'*) cat "$STATE/job_count" 2>/dev/null || echo 3 ;;
      *'"bad=" +'*) printf 'bad=%s\n' "$(sed 's/|.*//' "$STATE/failed_job")" ;;
      *) cat "$STATE/failed_job" ;;
    esac
    ;;
  attempt) cat "$STATE/attempt" 2>/dev/null || echo 1 ;;
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

checks=0
failures=0
fail() {
  printf 'train-push.test.sh: FAIL — %s\n' "$1" >&2
  if [ -n "${LAST_OUT:-}" ]; then
    printf '%s\n' "$LAST_OUT" | sed 's/^/    | /' >&2
  fi
  failures=$((failures + 1))
}
ok() { checks=$((checks + 1)); printf 'train-push.test.sh: ok — %s\n' "$1"; }

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
  mkdir -p "$dir/work/scripts/lib"
  cp "$SCRIPT_DIR/watch-ci.sh" "$dir/work/scripts/watch-ci.sh"
  cp "$SCRIPT_DIR/check-path-deps.py" "$dir/work/scripts/check-path-deps.py"
  cp "$SCRIPT_DIR/lib/operator-gh.sh" "$dir/work/scripts/lib/operator-gh.sh"
  cp "$SCRIPT_DIR/lib/workflow-gates.py" "$dir/work/scripts/lib/workflow-gates.py"
  git -C "$dir/work" add base.txt .github/workflows/tests.yml scripts
  git -C "$dir/work" commit -qm "base"
  git -C "$dir/work" remote add origin "$dir/origin.git"
  git -C "$dir/work" push -q origin "$DEFAULT_BRANCH"
  git -C "$dir/work" fetch -q origin
  git -C "$dir/work" remote set-head origin -a >/dev/null

  mkdir -p "$dir/ci-state"
  echo "4242" > "$dir/ci-state/run_id"
  echo "success" > "$dir/ci-state/conclusion"
  echo "completed" > "$dir/ci-state/recorded_status"
  echo "1" > "$dir/ci-state/attempt"
  echo "$DEFAULT_BRANCH" > "$dir/ci-state/default_branch"
  : > "$dir/ci-state/run_branch"
  : > "$dir/ci-state/failed_job"
  : > "$dir/ci-state/rerun_jobs"

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
    cd "${TRAIN_PUSH_TEST_CWD:-$dir/work}" &&
      PATH="$BIN_DIR:$PATH" \
      REPO="${TRAIN_PUSH_TEST_REPO-example/repo}" \
      TRAIN_PUSH_TEST_BRANCH="train/${1:-}" \
      OPERATOR_GH_FALLBACK_PATHS="$TMP_ROOT/no-such-fallback" \
      TRAIN_PUSH_TEST_STATE="$dir/ci-state" \
      WATCH_CI_RESOLVE_ATTEMPTS=1 \
       WATCH_CI_RESOLVE_SLEEP=0 \
       WATCH_CI_POLL_SLEEP=0.02 \
       TRAIN_PUSH_PARENT_ATTEMPTS="${TRAIN_PUSH_TEST_PARENT_ATTEMPTS:-200}" \
       TRAIN_PUSH_PARENT_SLEEP=0.02 \
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

# Isolated arm used to prove that the stale completed conclusion is rejected.
# Keeping this one assertion selectable lets mutation checks distinguish the
# old-run verdict from unrelated recovery assertions in the full suite.
test_same_sha_failed_rerun_message() {
  local dir
  dir="$(new_fixture same-sha-failed-rerun-message)"
  add_train_commit "$dir/work" "same-sha-failed-rerun-message"
  git -C "$dir/work" push -q origin "HEAD:refs/heads/train/same-sha-failed-rerun-message"
  echo "failure" > "$dir/ci-state/conclusion"
  echo "Unit / runner unavailable|9199" > "$dir/ci-state/failed_job"
  echo "failure|9199" > "$dir/ci-state/rerun_jobs"
  echo "success" > "$dir/ci-state/rerun_conclusion"
  run_train "$dir" same-sha-failed-rerun-message
  expect_out "sha already ran in https://github.com/example/repo/actions/runs/4242 (failure); rerunning its failed and cancelled jobs (attempt 2)" \
    "same-sha failed-run rerun result"
}

if [ "${TRAIN_PUSH_TEST_CASE:-}" = "same-sha-failed-rerun-message" ]; then
  test_same_sha_failed_rerun_message
  if [ "$failures" -ne 0 ]; then
    exit 1
  fi
  echo "train-push.test.sh: isolated same-sha failed rerun passed"
  exit 0
fi

# A push creates an independent, initially running CI record for that exact sha
# and branch. Only the test driver completes it; gh cannot invent a green run.
# Repushing a new sha creates another run, just as the real push workflow does.
cat > "$BIN_DIR/stack-gh.py" <<'PY'
import json, os, pathlib, re, sys
state = pathlib.Path(os.environ['TRAIN_PUSH_TEST_STATE'])
args = sys.argv[1:]
def option(name):
    return args[args.index(name) + 1] if name in args else ''
field, query = option('--json'), option('--jq')
runs = [json.loads(p.read_text()) for p in (state / 'runs').glob('*.json')]
if args[:2] == ['run', 'rerun']:
    (state / 'reruns').write_text('unexpected rerun\n')
    sys.exit(1)
if args[:2] == ['run', 'list']:
    sha = re.search(r'headSha=="([0-9a-f]+)"', query).group(1)
    branch = re.search(r'headBranch=="([^"]+)"', query).group(1)
    matches = [r for r in runs if r['sha'] == sha and r['branch'] == branch]
    if matches:
        print(max(matches, key=lambda r: r['id'])['id'])
    sys.exit(0)
if args[:2] != ['run', 'view']:
    sys.exit('unexpected fake CI query: ' + repr(args))
run = next(r for r in runs if str(r['id']) == args[2])
if '--log-failed' in args:
    print('FAIL [   0.10s] canned::stack_failure')
elif field == 'status,conclusion,jobs,url':
    jobs = [{
        'name': 'Unit',
        'databaseId': 9001,
        'conclusion': 'failure' if run['conclusion'] == 'failure' else 'success',
    }]
    print(json.dumps({
        'status': run['status'],
        'conclusion': run['conclusion'],
        'jobs': jobs,
        'url': 'https://github.com/example/repo/actions/runs/' + str(run['id']),
    }))
elif field == 'url':
    print('https://github.com/example/repo/actions/runs/' + str(run['id']))
elif field == 'status,conclusion':
    print(run['status'] + '\t' + run['conclusion'])
elif field in ('status', 'conclusion'):
    print(run[field])
elif field == 'jobs':
    if run['conclusion'] == 'failure':
        print('Unit|9001')
elif field == 'attempt':
    print(1)
else:
    sys.exit('unexpected fake CI field: ' + field)
PY

new_stack_fixture() {
  local dir
  dir="$(new_fixture "$1")"
  mkdir -p "$dir/ci-state/runs"
  cat > "$dir/origin.git/hooks/post-receive" <<HOOK
#!/usr/bin/env python3
import json, pathlib, sys
state = pathlib.Path('$dir/ci-state')
for line in sys.stdin:
    old, sha, ref = line.split()
    if ref == 'refs/heads/$DEFAULT_BRANCH':
        with (state / 'landings').open('a') as f:
            f.write(sha + '\\n')
    if not ref.startswith('refs/heads/train/') or set(sha) == {'0'}:
        continue
    rid = len(list((state / 'runs').glob('*.json'))) + 5000
    run = dict(id=rid, sha=sha, branch=ref[len('refs/heads/'):], status='in_progress', conclusion='')
    path = state / 'runs' / (str(rid) + '.json')
    tmp = path.with_suffix('.tmp')
    tmp.write_text(json.dumps(run))
    tmp.replace(path)
HOOK
  chmod +x "$dir/origin.git/hooks/post-receive"
  printf '%s\n' "$dir"
}

stack_worktree() {
  git -C "$1/work" worktree add -qb "$2" "$1/$2" "$3"
}

stack_commit() {
  echo "$2" > "$1/$2.txt"
  git -C "$1" add "$2.txt"
  git -C "$1" commit -qm "$2"
}

complete_stack_ci() {
  python3 - "$1/ci-state" "$2" "$3" <<'PY'
import json, pathlib, sys
state, branch, conclusion = sys.argv[1:]
runs = [p for p in pathlib.Path(state, 'runs').glob('*.json')
        if json.loads(p.read_text())['branch'] == 'train/' + branch]
assert runs, 'cannot complete a run that a push has not started'
path = max(runs, key=lambda p: json.loads(p.read_text())['id'])
run = json.loads(path.read_text())
run.update(status='completed', conclusion=conclusion)
tmp = path.with_suffix('.tmp')
tmp.write_text(json.dumps(run))
tmp.replace(path)
PY
}

# Background only inside the harness: the driver has useful work (completing
# independent runs) before joining every train. A bounded poll prevents a broken
# train from leaving this suite hanging forever.
start_stack_train() {
  local dir="$1" worktree="$2" name="$3"
  shift 3
  (
    TRAIN_PUSH_TEST_CWD="$dir/$worktree" run_train "$dir" "$name" "$@"
    printf '%s\n' "$LAST_OUT" > "$dir/$name.out"
    printf '%s\n' "$LAST_RC" > "$dir/$name.rc"
  ) &
  STACK_PID=$!
}

wait_stack_push() {
  local i
  for i in $(seq 1 100); do
    [ -n "$(origin_ref "$1" "refs/heads/train/$2")" ] &&
      [ -n "$(python3 - "$1/ci-state/runs" "$2" <<'PY'
import json, pathlib, sys
print('yes' if any(json.loads(p.read_text())['branch'] == 'train/' + sys.argv[2]
                   for p in pathlib.Path(sys.argv[1]).glob('*.json')) else '')
PY
)" ] && return 0
    [ -f "$1/$2.rc" ] && break
    sleep 0.02
  done
  LAST_OUT="$(cat "$1/$2.out" 2>/dev/null || true)"
  fail "stack fixture $2 never pushed and started CI"
  return 1
}

join_stack_train() {
  wait "$2" || true
  LAST_OUT="$(cat "$1/$3.out")"
  LAST_RC="$(cat "$1/$3.rc")"
}

stack_child_fixture() {
  local dir base
  dir="$(new_stack_fixture "$1")"
  base="$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
  stack_worktree "$dir" parent "$base"
  stack_commit "$dir/parent" parent
  git -C "$dir/parent" push -q origin HEAD:refs/heads/train/parent
  stack_worktree "$dir" child "$(git -C "$dir/parent" rev-parse HEAD)"
  stack_commit "$dir/child" child
  printf '%s\n' "$dir"
}

test_stack_base_not_on_main() {
  local dir pid base
  dir="$(stack_child_fixture stack-base)"
  base="$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
  TRAIN_PUSH_TEST_PARENT_ATTEMPTS=4 start_stack_train "$dir" child child --on parent
  pid="$STACK_PID"
  if wait_stack_push "$dir" child; then complete_stack_ci "$dir" child success; fi
  join_stack_train "$dir" "$pid" child
  if [ "$LAST_RC" = 2 ] && [ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$base" ] &&
    [[ "$LAST_OUT" == *"waiting for parent parent"* ]] && [[ "$LAST_OUT" == *"wait limit"* ]]; then
    ok "stacked child cannot land before its base is on main"
  else
    fail "stacked child cannot land before its base is on main"
  fi
}

test_stack_parent_red() {
  local dir pid base
  dir="$(stack_child_fixture stack-red)"
  base="$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
  complete_stack_ci "$dir" parent failure
  start_stack_train "$dir" child child --on parent
  pid="$STACK_PID"
  if wait_stack_push "$dir" child; then complete_stack_ci "$dir" child success; fi
  join_stack_train "$dir" "$pid" child
  if [ "$LAST_RC" = 2 ] && [[ "$LAST_OUT" == *"CI green:"* ]] &&
    [[ "$LAST_OUT" == *"parent parent is red; fix and repush it, then restack child"* ]] &&
    [ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$base" ]; then
    ok "parent red blocks a green stacked child with the restack remedy"
  else
    fail "parent red blocks a green stacked child with the restack remedy"
  fi
}

test_stack_changed_parent() {
  local dir pid old_parent new_parent old_child child_sha
  dir="$(stack_child_fixture stack-changed)"
  old_parent="$(git -C "$dir/parent" rev-parse HEAD)"
  old_child="$(git -C "$dir/child" rev-parse HEAD)"
  # Record the child's original stack, then finish its own run while its parent
  # is still running. The timeout is recoverable; CI keeps the green result.
  TRAIN_PUSH_TEST_PARENT_ATTEMPTS=4 start_stack_train "$dir" child child --on parent
  pid="$STACK_PID"
  if wait_stack_push "$dir" child; then complete_stack_ci "$dir" child success; fi
  join_stack_train "$dir" "$pid" child
  stack_commit "$dir/parent" parent-fix
  new_parent="$(git -C "$dir/parent" rev-parse HEAD)"
  git -C "$dir/parent" push -q origin HEAD:refs/heads/train/parent
  complete_stack_ci "$dir" parent success
  TRAIN_PUSH_TEST_CWD="$dir/parent" run_train "$dir" parent --land
  expect_rc 0 "repushed parent lands its new green sha"
  TRAIN_PUSH_TEST_CWD="$dir/child" run_train "$dir" child --land
  if [ "$LAST_RC" = 2 ] && [[ "$LAST_OUT" == *"--restack"* ]] &&
    [[ "$LAST_OUT" == *"base $old_parent"* ]] &&
    [ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$new_parent" ]; then
    ok "a parent landed at a different sha refuses the child's old green"
  else
    fail "a parent landed at a different sha refuses the child's old green"
  fi
  # Restack removes the old parent range, replays only child commits, and must
  # push a new sha with a fresh run instead of spending the previous green.
  rm -f "$dir/child.rc" "$dir/child.out"
  start_stack_train "$dir" child child --restack
  pid="$STACK_PID"
  for _ in $(seq 1 100); do
    child_sha="$(origin_ref "$dir" refs/heads/train/child)"
    [ "$child_sha" != "$old_child" ] &&
      [ "$(python3 - "$dir/ci-state/runs" <<'PY'
import json, pathlib, sys
print(sum(json.loads(p.read_text())['branch'] == 'train/child' for p in pathlib.Path(sys.argv[1]).glob('*.json')))
PY
)" = 2 ] && break
    [ -f "$dir/child.rc" ] && ! kill -0 "$pid" 2>/dev/null && break
    sleep 0.02
  done
  complete_stack_ci "$dir" child success
  join_stack_train "$dir" "$pid" child
  expect_rc 0 "restack repushes, retests, and lands on the parent's new sha"
  [ "$(git -C "$dir/child" rev-list --count "$new_parent..HEAD")" = 1 ] ||
    fail "restack replayed parent commits instead of only the child"
  [ -f "$dir/child/parent-fix.txt" ] || fail "restack lost the parent's fix"
}

test_stack_chain() {
  local dir base p1 p2 p3 s1 s2 s3
  dir="$(new_stack_fixture stack-chain)"
  base="$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
  stack_worktree "$dir" parent "$base"
  stack_commit "$dir/parent" parent
  s1="$(git -C "$dir/parent" rev-parse HEAD)"
  start_stack_train "$dir" parent parent; p1="$STACK_PID"
  if ! wait_stack_push "$dir" parent; then join_stack_train "$dir" "$p1" parent; return; fi
  stack_worktree "$dir" child "$s1"
  stack_commit "$dir/child" child
  s2="$(git -C "$dir/child" rev-parse HEAD)"
  # No --on here: detect the nearest pending train tip as the parent.
  start_stack_train "$dir" child child; p2="$STACK_PID"
  if ! wait_stack_push "$dir" child; then
    complete_stack_ci "$dir" parent success
    join_stack_train "$dir" "$p1" parent; join_stack_train "$dir" "$p2" child; return
  fi
  stack_worktree "$dir" grandchild "$s2"
  stack_commit "$dir/grandchild" grandchild
  s3="$(git -C "$dir/grandchild" rev-parse HEAD)"
  start_stack_train "$dir" grandchild grandchild; p3="$STACK_PID"
  if wait_stack_push "$dir" grandchild; then complete_stack_ci "$dir" grandchild success; fi
  complete_stack_ci "$dir" child success
  # Green descendants cannot land while their parent is running. Wait until
  # the child has actually reached the landing guard (not just the CI watch).
  for _ in $(seq 1 100); do
    grep -q 'headBranch=="train/parent"' "$dir/ci-state/gh-calls" && break
    sleep 0.02
  done
  if [ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$base" ] &&
    kill -0 "$p1" && kill -0 "$p2" && kill -0 "$p3"; then
    ok "stacked child waits while parent CI runs; all three CI runs started in parallel"
  else
    fail "stacked child waits while parent CI runs; all three CI runs started in parallel"
  fi
  complete_stack_ci "$dir" parent success
  join_stack_train "$dir" "$p1" parent; expect_rc 0 "stack chain parent lands"
  join_stack_train "$dir" "$p2" child; expect_rc 0 "stacked child lands without a rerun after parent lands"
  expect_out "waiting for parent parent" "stacked child reports its bounded parent wait"
  expect_no_out "round 2" "stacked child spends its original green without requeue"
  join_stack_train "$dir" "$p3" grandchild; expect_rc 0 "three-deep grandchild lands"
  expect_out "detected parent child at $s2" "auto-detection chooses the nearest pending train in a three-deep chain"
  if [ "$(cat "$dir/ci-state/landings")" = "$(printf '%s\n' "$s1" "$s2" "$s3")" ] &&
    [ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$s3" ] &&
    [ "$(python3 - "$dir/ci-state/runs" <<'PY'
import pathlib, sys
print(len(list(pathlib.Path(sys.argv[1]).glob('*.json'))))
PY
)" = 3 ] &&
    [ ! -s "$dir/ci-state/reruns" ]; then
    ok "three-deep chain lands in order at the exact tested shas with one CI run each"
  else
    fail "three-deep chain lands in order at the exact tested shas with one CI run each"
  fi
}

test_stack_abandoned_parent() {
  local dir pid base old_child
  dir="$(stack_child_fixture stack-abandoned)"
  base="$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
  old_child="$(git -C "$dir/child" rev-parse HEAD)"
  TRAIN_PUSH_TEST_PARENT_ATTEMPTS=4 start_stack_train "$dir" child child --on parent
  pid="$STACK_PID"
  if wait_stack_push "$dir" child; then complete_stack_ci "$dir" child success; fi
  join_stack_train "$dir" "$pid" child
  git -C "$dir/parent" push -q origin --delete train/parent
  TRAIN_PUSH_TEST_CWD="$dir/child" run_train "$dir" child --land
  if [ "$LAST_RC" = 2 ] && [[ "$LAST_OUT" == *"parent parent is abandoned"* ]] &&
    [[ "$LAST_OUT" == *"--restack"* ]] &&
    [ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$base" ]; then
    ok "an abandoned parent blocks the child's recorded green"
  else
    fail "an abandoned parent blocks the child's recorded green"
  fi
  rm -f "$dir/child.rc" "$dir/child.out"
  start_stack_train "$dir" child child --restack
  pid="$STACK_PID"
  for _ in $(seq 1 100); do
    [ "$(origin_ref "$dir" refs/heads/train/child)" != "$old_child" ] && break
    [ -f "$dir/child.rc" ] && break
    sleep 0.02
  done
  complete_stack_ci "$dir" child success
  join_stack_train "$dir" "$pid" child
  if [ "$LAST_RC" = 0 ] && [ "$(git -C "$dir/child" rev-list --count "$base..HEAD")" = 1 ] &&
    [ ! -f "$dir/child/parent.txt" ] && [ -f "$dir/child/child.txt" ]; then
    ok "restacking an abandoned parent onto main removes only the parent range and retests"
  else
    fail "restacking an abandoned parent onto main removes only the parent range and retests"
  fi
}

test_stack_name_lock() {
  local dir pid base
  dir="$(new_stack_fixture stack-name-lock)"
  base="$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
  stack_worktree "$dir" parent "$base"
  stack_commit "$dir/parent" parent
  start_stack_train "$dir" parent parent
  pid="$STACK_PID"
  if ! wait_stack_push "$dir" parent; then join_stack_train "$dir" "$pid" parent; return; fi
  stack_worktree "$dir" duplicate "$(git -C "$dir/parent" rev-parse HEAD)"
  TRAIN_PUSH_TEST_CWD="$dir/duplicate" run_train "$dir" parent
  if [ "$LAST_RC" = 2 ] && [[ "$LAST_OUT" == *"train parent is RUNNING as pid"* ]]; then
    ok "one train name cannot be driven concurrently from different worktrees"
  else
    fail "one train name cannot be driven concurrently from different worktrees"
  fi
  complete_stack_ci "$dir" parent success
  join_stack_train "$dir" "$pid" parent
  expect_rc 0 "the original train retains its name lock and lands"
}

# Name-lock recovery must be serialized in the shared git directory, not in
# whichever linked worktree happens to be recovering it. An interrupted reaper
# is an intentional fail-closed condition; another worktree must not bypass it.
test_stack_name_lock_recovery() {
  local dir base key lock
  dir="$(new_fixture stack-name-recovery)"
  base="$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
  stack_worktree "$dir" child "$base"
  stack_commit "$dir/child" child
  key="$(printf '%s' child | git -C "$dir/work" hash-object --stdin)"
  lock="$dir/work/.git/train-push-locks/by-name-$key"
  mkdir -p "$lock/held" "$lock/reap"
  printf '2147483000 gone\n' > "$lock/held/owner"
  printf '2147483000\n' > "$lock/reap/owner"
  TRAIN_PUSH_TEST_CWD="$dir/child" run_train "$dir" child
  if [ "$LAST_RC" = 2 ] && [[ "$LAST_OUT" == *"train lock recovery left behind"* ]] &&
    [ -f "$lock/reap/owner" ] && [ -f "$lock/held/owner" ] &&
    [ -z "$(origin_ref "$dir" refs/heads/train/child)" ]; then
    ok "a linked worktree cannot bypass interrupted shared name-lock recovery"
  else
    fail "a linked worktree cannot bypass interrupted shared name-lock recovery"
  fi
}

# Inject the errors Git emits when linked worktrees update shared remote refs.
# The real fetch still runs on successful attempts; auth/permission failures
# are separate controls and must never be mistaken for a ref-update race.
install_fetch_faults() {
  local dir="$1" real_git
  real_git="$(command -v git)"
  mkdir -p "$dir/fixturebin"
  cat > "$dir/fixturebin/git" <<SHIM
#!/usr/bin/env bash
if [ -f "$dir/ci-state/polling" ]; then
  for arg in "\$@"; do
    if [ "\$arg" = "refs/heads/train/parent" ]; then
      case "\$(cat "$dir/ci-state/fetch-fault")" in
        remote-error) echo 'fatal: Authentication failed for origin' >&2; exit 128 ;;
        remote-empty) exit 0 ;;
      esac
    fi
  done
fi
for arg in "\$@"; do
  if [ "\$arg" = push ] && [ "\$(cat "$dir/ci-state/fetch-fault")" = recover ] &&
    [ ! -f "$dir/ci-state/successful-fetches" ]; then
    touch "$dir/ci-state/pushed-before-fetch-recovery"
  fi
  if [ "\$arg" = fetch ]; then
    count=0
    [ ! -f "$dir/ci-state/fetch-count" ] || read -r count < "$dir/ci-state/fetch-count"
    count=\$((count + 1))
    printf '%s\\n' "\$count" > "$dir/ci-state/fetch-count"
    mode=\$(cat "$dir/ci-state/fetch-fault")
    case "\$mode" in
      recover)
        if [ "\$count" = 1 ]; then
          echo "error: cannot lock ref 'refs/remotes/origin/$DEFAULT_BRANCH': is at 1111111111111111111111111111111111111111 but expected 2222222222222222222222222222222222222222" >&2
          exit 1
        elif [ "\$count" = 2 ]; then
          echo "error: cannot lock ref 'refs/remotes/origin/$DEFAULT_BRANCH': Unable to create 'common/refs/remotes/origin/$DEFAULT_BRANCH.lock': File exists." >&2
          exit 1
        fi
        ;;
      persistent)
        echo "error: cannot lock ref 'refs/remotes/origin/$DEFAULT_BRANCH': is at 1111111111111111111111111111111111111111 but expected 2222222222222222222222222222222222222222" >&2
        exit 1
        ;;
      permission)
        echo "error: cannot lock ref 'refs/remotes/origin/$DEFAULT_BRANCH': Unable to create 'common/refs/remotes/origin/$DEFAULT_BRANCH.lock': Permission denied" >&2
        exit 1
        ;;
      transport)
        echo 'fatal: Authentication failed for origin' >&2
        exit 128
        ;;
      polling)
        if [ -f "$dir/ci-state/polling" ]; then
          echo fetched > "$dir/ci-state/fetched-while-polling"
          echo "error: cannot lock ref 'refs/remotes/origin/$DEFAULT_BRANCH': is at 1111111111111111111111111111111111111111 but expected 2222222222222222222222222222222222222222" >&2
          exit 1
        fi
        ;;
    esac
    printf '%s\\n' "\$count" >> "$dir/ci-state/successful-fetches"
    break
  fi
done
exec "$real_git" "\$@"
SHIM
  chmod +x "$dir/fixturebin/git"
}

test_fetch_race_retry() {
  local dir
  dir="$(new_fixture fetch-recovery)"
  stack_worktree "$dir" child "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
  stack_commit "$dir/child" child
  echo recover > "$dir/ci-state/fetch-fault"
  install_fetch_faults "$dir"
  PATH="$dir/fixturebin:$PATH" TRAIN_PUSH_TEST_CWD="$dir/child" run_train "$dir" child
  if [ "$LAST_RC" = 0 ] && [[ "$LAST_OUT" == *"shared ref update race; retrying"* ]] &&
    [ "$(head -1 "$dir/ci-state/successful-fetches" 2>/dev/null)" = 3 ] &&
    [ ! -f "$dir/ci-state/pushed-before-fetch-recovery" ] &&
    [ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$(git -C "$dir/child" rev-parse HEAD)" ]; then
    ok "a linked-worktree train retries ref-lock and compare-and-swap fetch failures and lands"
  else
    fail "a linked-worktree train retries ref-lock and compare-and-swap fetch failures and lands"
  fi
}

test_fetch_failure_boundaries() {
  local dir mode want
  for mode in persistent permission transport; do
    dir="$(new_fixture "fetch-$mode")"
    add_train_commit "$dir/work" "fetch-$mode"
    echo "$mode" > "$dir/ci-state/fetch-fault"
    install_fetch_faults "$dir"
    PATH="$dir/fixturebin:$PATH" run_train "$dir" "fetch-$mode"
    want=1
    [ "$mode" != persistent ] || want=5
    if [ "$LAST_RC" = 2 ] && [ "$(cat "$dir/ci-state/fetch-count")" = "$want" ] &&
      [ -z "$(origin_ref "$dir" "refs/heads/train/fetch-$mode")" ]; then
      ok "$mode fetch failures refuse before push after exactly $want attempt(s)"
    else
      fail "$mode fetch failures refuse before push after exactly $want attempt(s)"
    fi
  done
}

test_stack_poll_without_fetch() {
  local dir pid
  dir="$(stack_child_fixture stack-polling)"
  echo polling > "$dir/ci-state/fetch-fault"
  install_fetch_faults "$dir"
  PATH="$dir/fixturebin:$PATH" TRAIN_PUSH_TEST_PARENT_ATTEMPTS=4 start_stack_train "$dir" child child --on parent
  pid="$STACK_PID"
  if wait_stack_push "$dir" child; then
    touch "$dir/ci-state/polling"
    complete_stack_ci "$dir" child success
  fi
  join_stack_train "$dir" "$pid" child
  if [ "$LAST_RC" = 2 ] && [[ "$LAST_OUT" == *"parent parent has not landed"* ]] &&
    [ ! -f "$dir/ci-state/fetched-while-polling" ]; then
    ok "a stacked child polls remote parent refs without fetching shared tracking refs"
  else
    fail "a stacked child polls remote parent refs without fetching shared tracking refs"
  fi
}

test_stack_unseen_remote_head() {
  local dir pid old_main new_main=""
  dir="$(stack_child_fixture stack-unseen-head)"
  old_main="$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
  TRAIN_PUSH_TEST_PARENT_ATTEMPTS=4 start_stack_train "$dir" child child --on parent
  pid="$STACK_PID"
  if wait_stack_push "$dir" child; then
    # The peer is a separate clone, so this object is not yet in the linked
    # worktrees' shared object database when the child starts polling.
    advance_origin_main "$dir" peer-after-child-push
    new_main="$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
    if git -C "$dir/child" cat-file -e "$new_main^{commit}" 2>/dev/null; then
      fail "unseen-head fixture unexpectedly already has the peer's commit"
    fi
    complete_stack_ci "$dir" child success
  fi
  join_stack_train "$dir" "$pid" child
  if [ "$LAST_RC" = 2 ] && [[ "$LAST_OUT" == *"parent parent has not landed"* ]] &&
    git -C "$dir/child" cat-file -e "$new_main^{commit}" 2>/dev/null &&
    [ "$(git -C "$dir/child" rev-parse "origin/$DEFAULT_BRANCH")" = "$old_main" ]; then
    ok "a child downloads an unseen remote head without changing shared tracking refs"
  else
    fail "a child downloads an unseen remote head without changing shared tracking refs"
  fi
}

test_stack_remote_read_failures() {
  local dir pid mode
  for mode in remote-error remote-empty; do
    dir="$(stack_child_fixture "stack-$mode")"
    echo "$mode" > "$dir/ci-state/fetch-fault"
    install_fetch_faults "$dir"
    PATH="$dir/fixturebin:$PATH" TRAIN_PUSH_TEST_PARENT_ATTEMPTS=4 start_stack_train "$dir" child child --on parent
    pid="$STACK_PID"
    if wait_stack_push "$dir" child; then
      touch "$dir/ci-state/polling"
      complete_stack_ci "$dir" child success
    fi
    join_stack_train "$dir" "$pid" child
    if [ "$LAST_RC" = 4 ] && [[ "$LAST_OUT" == *"CI green, but landing failed"* ]] &&
      [ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$(git -C "$dir/child" rev-parse HEAD~2)" ]; then
      ok "$mode while reading parent heads fails closed without landing"
    else
      fail "$mode while reading parent heads fails closed without landing"
    fi
  done
}

test_ci_conclusion_guard() {
  local dir base
  dir="$(new_fixture ci-conclusion)"
  base="$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
  add_train_commit "$dir/work" ci-conclusion
  # watch-ci's all-jobs-passed fallback can return zero for this non-success
  # summary. The landing contract still requires the run to conclude success.
  echo cancelled > "$dir/ci-state/conclusion"
  run_train "$dir" ci-conclusion
  if [ "$LAST_RC" = 2 ] && [[ "$LAST_OUT" == *"did not conclude success; not landing"* ]] &&
    [ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$base" ]; then
    ok "a non-success run summary cannot authorize landing even if the watcher returns zero"
  else
    fail "a non-success run summary cannot authorize landing even if the watcher returns zero"
  fi
}

run_stack_tests() {
  test_stack_base_not_on_main
  test_stack_parent_red
  test_stack_changed_parent
  test_stack_abandoned_parent
  test_stack_name_lock
  test_stack_name_lock_recovery
  test_stack_poll_without_fetch
  test_stack_unseen_remote_head
  test_stack_remote_read_failures
  test_fetch_race_retry
  test_fetch_failure_boundaries
  test_stack_chain
  test_ci_conclusion_guard
}
case "${TRAIN_PUSH_TEST_CASE:-}" in
  stack-base) test_stack_base_not_on_main ;;
  stack-red) test_stack_parent_red ;;
  stack-changed) test_stack_changed_parent ;;
  stack-chain) test_stack_chain ;;
  stack-abandoned) test_stack_abandoned_parent ;;
  stack-name-lock) test_stack_name_lock ;;
  stack-name-recovery) test_stack_name_lock_recovery ;;
  stack-polling) test_stack_poll_without_fetch ;;
  stack-unseen-head) test_stack_unseen_remote_head ;;
  stack-remote-errors) test_stack_remote_read_failures ;;
  fetch-retry) test_fetch_race_retry ;;
  fetch-boundaries) test_fetch_failure_boundaries ;;
  ci-conclusion) test_ci_conclusion_guard ;;
  stacked) run_stack_tests ;;
esac
if [[ "${TRAIN_PUSH_TEST_CASE:-}" == stack* ]] || [[ "${TRAIN_PUSH_TEST_CASE:-}" == fetch-* ]] || [ "${TRAIN_PUSH_TEST_CASE:-}" = ci-conclusion ]; then
  [ "$failures" = 0 ] || exit 1
  echo "train-push.test.sh: stacked cases passed ($checks checks)"
  exit 0
fi
run_stack_tests

# --- refusal: no train name ------------------------------------------------
dir="$(new_fixture no-path-checker)"
add_train_commit "$dir/work" "no-path-checker"
git -C "$dir/work" rm -q scripts/check-path-deps.py
git -C "$dir/work" commit -qm "drop the path checker"
run_train "$dir" no-path-checker
expect_rc 2 "a missing path-dependency checker refuses"
expect_out "scripts/check-path-deps.py not found" "the refusal names the missing checker"
expect_no_out "dependency path resolves outside" "a missing checker is not reported as a path verdict"

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

# --- refusal: a later `!` pattern excludes the train ref --------------------
# GitHub evaluates a branches list in order: `train/**` includes the ref and a
# later `!train/blocked/**` excludes it again. A first-match reading approved
# the push and the train hung waiting for a run that could never appear,
# which reads as a CI outage rather than a misconfiguration (BROCA's
# specimen). The trigger probe cannot catch it: its own ref is included.
dir="$(new_fixture excluded)"
write_tests_workflow "$dir/work" "      - $DEFAULT_BRANCH
      - 'train/**'
      - '!train/blocked/**'"
git -C "$dir/work" add .github/workflows/tests.yml
git -C "$dir/work" commit -qm "excluding trigger"
run_train "$dir" blocked/release
expect_rc 2 "a train ref a later ! pattern excludes refuses"
[ -z "$(origin_ref "$dir" refs/heads/train/blocked/release)" ] ||
  fail "excluded ref refusal still pushed a train branch"
# Control: a sibling ref outside the exclusion is still accepted by the scan
# (the run itself is not watched here; the first-run probe rows cover that).
python3 "$SCRIPT_DIR/lib/workflow-gates.py" --train-ref train/ok --tests-workflow "$dir/work/.github/workflows/tests.yml" 2>/dev/null | grep -q '^trigger|ok$' ||
  fail "a ref the exclusion does not cover must still trigger"
# Re-inclusion: a positive pattern after the `!` admits the ref again. Both
# lifted implementations support it because the docs say so; without a row
# it is the arm that rots quietly.
write_tests_workflow "$dir/work" "      - $DEFAULT_BRANCH
      - 'train/**'
      - '!train/blocked/**'
      - 'train/blocked/allowed'"
python3 "$SCRIPT_DIR/lib/workflow-gates.py" --train-ref train/blocked/allowed --tests-workflow "$dir/work/.github/workflows/tests.yml" 2>/dev/null | grep -q '^trigger|ok$' ||
  fail "a later positive pattern must re-include a ref an earlier ! excluded"
python3 "$SCRIPT_DIR/lib/workflow-gates.py" --train-ref train/blocked/other --tests-workflow "$dir/work/.github/workflows/tests.yml" 2>/dev/null | grep -q '^trigger|missing$' ||
  fail "re-inclusion must be exact, not a widening of the exclusion"

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

# The proof is one fact per REPOSITORY. A linked worktree has its own git dir
# (.git/worktrees/<name>), so a marker kept there made every fresh worktree
# re-prove the trigger: one probe push and one extra CI run per train.
git -C "$dir/work" worktree add -q -b wt-train "$dir/wt" HEAD
add_train_commit "$dir/wt" "probe-from-worktree"
TRAIN_PUSH_TEST_CWD="$dir/wt" run_train "$dir" probe-wt
expect_rc 0 "a train from a linked worktree lands"
expect_no_out "first train in this repo" \
  "a linked worktree reads the repository's trigger proof"
[ ! -e "$(git -C "$dir/wt" rev-parse --absolute-git-dir)/train-push-proven" ] ||
  fail "the proof was written into the worktree's own git dir"

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

# --- no warning: the shared content-addressed dispatcher set either --------
dir="$(new_fixture keyed-dispatcher)"
add_train_commit "$dir/work" "keyed-dispatcher"
managed_hooks="$TMP_ROOT/fake-cache/aft/git-hooks/0123456789abcdef0123456789abcdef"
mkdir -p "$managed_hooks"
printf '#!/bin/sh\nexit 0\n' > "$managed_hooks/pre-push"
chmod +x "$managed_hooks/pre-push"
git -C "$dir/work" config core.hooksPath "$managed_hooks"
run_train "$dir" keyed-dispatcher
expect_rc 0 "the shared managed dispatcher does not block a train"
expect_no_out "pre-push hook" "the shared managed dispatcher alone is not warned about"

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

# --- red that is skew by STEP name: named, on single-job CI ------------------
# A seat whose CI is one job per platform names the job after the platform, so
# the job-name arm can never match; the lock check is a step inside it.
dir="$(new_fixture skew-step)"
add_train_commit "$dir/work" "skew-step"
echo "Build and test (macOS)|9003" > "$dir/ci-state/failed_job"
echo "failure" > "$dir/ci-state/conclusion"
printf 'Build\nCargo.lock drift check\n' > "$dir/ci-state/job_steps"
run_train "$dir" skew-step
expect_rc 1 "a step-named skew red exits 1"
expect_out "red is a version/lock skew, not contention: this terminates in a lockfile bump commit, not a retry" \
  "a failing step name matching the skew pattern is classed as skew"
expect_out "Build and test (macOS)" "step-skew red still names the job"

# --- red that is skew by LOG TAIL: a lock check that is one phase in a step --
dir="$(new_fixture skew-log)"
add_train_commit "$dir/work" "skew-log"
echo "Build and test (macOS)|9004" > "$dir/ci-state/failed_job"
echo "failure" > "$dir/ci-state/conclusion"
printf 'Build\nTest\n' > "$dir/ci-state/job_steps"
printf 'compile ok\nunit tests ok\nerror: Cargo.lock has drifted from the committed manifests; run cargo update -w\n' > "$dir/ci-state/log_tail"
run_train "$dir" skew-log
expect_rc 1 "a log-tail skew red exits 1"
expect_out "red is a version/lock skew, not contention: this terminates in a lockfile bump commit, not a retry" \
  "a lockfile drift line in the failing log's tail is classed as skew"

# --- red that is a plain test failure: never classed as skew ----------------
dir="$(new_fixture skew-not)"
add_train_commit "$dir/work" "skew-not"
echo "Unit (ubuntu-latest)|9005" > "$dir/ci-state/failed_job"
echo "failure" > "$dir/ci-state/conclusion"
printf 'Build\nTest\n' > "$dir/ci-state/job_steps"
printf 'test write_ledger::tests::census ... FAILED\nerror: test failed, to rerun pass `-p agent-file-tools --lib`\n' > "$dir/ci-state/log_tail"
run_train "$dir" skew-not
expect_rc 1 "a plain test-failure red exits 1"
expect_no_out "version/lock skew" "a plain test failure is never classed as skew"
expect_out "Unit (ubuntu-latest)" "test-failure red still names the job"
# An early Cargo.lock mention in the log (a build line) must not class a later
# test failure as skew either: only the tail is judged.
dir="$(new_fixture skew-early)"
add_train_commit "$dir/work" "skew-early"
echo "Unit (ubuntu-latest)|9006" > "$dir/ci-state/failed_job"
echo "failure" > "$dir/ci-state/conclusion"
printf 'Build\nTest\n' > "$dir/ci-state/job_steps"
{
  printf 'checking Cargo.lock against manifests\n'
  i=0
  while [ "$i" -lt 60 ]; do printf 'Compiling crate%d v1.0.0\n' "$i"; i=$((i + 1)); done
  printf 'test integration::watcher ... FAILED\n'
} > "$dir/ci-state/log_tail"
run_train "$dir" skew-early
expect_rc 1 "an early lock mention with a failing tail exits 1"
expect_no_out "version/lock skew" "a lockfile line scrolled out of the tail is not skew"

# --- refusal: dirty tree ---------------------------------------------------
dir="$(new_fixture dirty)"
echo "uncommitted" > "$dir/work/base.txt"
run_train "$dir" dirty
expect_rc 2 "dirty tree refuses"
expect_out "working tree is not clean" "dirty tree names the reason"
[ -z "$(origin_ref "$dir" refs/heads/train/dirty)" ] ||
  fail "dirty tree refusal still pushed a train branch"

# --- Cargo.lock drift from an editor's cargo run: restored, not refused ----
# A lock with one path dependency (no `source =`) and one registry
# dependency, committed on main. Only a lone version bump of the path
# dependency counts as drift; everything else is the operator's work.
add_cargo_lock() {
  local work="$1"
  cat > "$work/Cargo.lock" <<'LOCK'
# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "pathdep"
version = "0.17.45"
dependencies = [
 "regdep",
]

[[package]]
name = "regdep"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"

[[package]]
name = "twin"
version = "0.2.1"
source = "git+https://example.invalid/commons.git?rev=abc#abc"

[[package]]
name = "twin"
version = "0.2.2"
LOCK
  git -C "$work" add Cargo.lock
  git -C "$work" commit -qm "lock"
  git -C "$work" push -q origin "$DEFAULT_BRANCH"
}

dir="$(new_fixture lockdrift)"
add_cargo_lock "$dir/work"
add_train_commit "$dir/work" "lockdrift"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
# Two path-dependency versions move, including the path copy of a crate that
# also has a git copy of the same name; the git copy's block is untouched.
sed -i.bak -e 's/^version = "0.17.45"$/version = "0.17.48"/' -e 's/^version = "0.2.2"$/version = "0.2.3"/' "$dir/work/Cargo.lock"
rm -f "$dir/work/Cargo.lock.bak"
run_train "$dir" lockdrift
expect_rc 0 "path-dependency-only lock drift is restored and the train lands"
expect_out "restored Cargo.lock: sibling path-dependency drift in pathdep,twin" "the restore names every drifted package"
expect_no_out "working tree is not clean" "lock drift alone is not a dirty tree"
[ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$train_sha" ] ||
  fail "lock drift landed a sha other than the committed one"
git -C "$dir/work" diff --quiet -- Cargo.lock ||
  fail "lock drift restore left Cargo.lock modified"

# --- refusal: a real change FIRST in a diff too large for the pipe buffer --
# ASTRO's measured inversion: the classifier's grep pipeline ended in -qv,
# which exits on the first non-version line; with thousands of version lines
# still to come the writers took SIGPIPE, the pipeline returned 141 under
# pipefail, and the working-tree change was classified as pure drift and
# RESTORED. The small fixtures above never hit it because the writer finished
# before grep closed the pipe. This arm puts the checksum change first and
# 20,000 source-less version bumps after it, and must still refuse.
dir="$(new_fixture lockbig)"
add_cargo_lock "$dir/work"
{
  i=0
  while [ "$i" -lt 20000 ]; do
    printf '\n[[package]]\nname = "bulk%d"\nversion = "2.0.0"\n' "$i"
    i=$((i + 1))
  done
} >> "$dir/work/Cargo.lock"
git -C "$dir/work" add Cargo.lock
git -C "$dir/work" commit -qm "big lock"
git -C "$dir/work" push -q origin "$DEFAULT_BRANCH"
add_train_commit "$dir/work" "lockbig"
# The real change is a dependency-list edit on the FIRST block (pathdep is
# source-less, so the per-package ownership check cannot catch it); every
# line after it is a source-less version bump the classifier accepts. Only
# the non-version-line scan stands between this and a restore.
sed -i.bak -e 's/^ "regdep",$/ "regdep-renamed",/' -e 's/^version = "2.0.0"$/version = "2.0.1"/' "$dir/work/Cargo.lock"
rm -f "$dir/work/Cargo.lock.bak"
run_train "$dir" lockbig
expect_rc 2 "a dependency-list change first in a 40k-line diff still refuses"
expect_out "working tree is not clean" "large diff with a real change refuses as a dirty tree"
expect_no_out "restored Cargo.lock" "the large diff is never restored over the operator's change"
git -C "$dir/work" diff --quiet -- Cargo.lock && fail "the operator's lock change was discarded"

# --- refusal: a lock change that touches a source or checksum line ---------
dir="$(new_fixture locksource)"
add_cargo_lock "$dir/work"
add_train_commit "$dir/work" "locksource"
sed -i.bak -e 's/^version = "1.0.0"$/version = "1.0.1"/' -e 's/^checksum = "0*"$/checksum = "1111111111111111111111111111111111111111111111111111111111111111"/' "$dir/work/Cargo.lock"
rm -f "$dir/work/Cargo.lock.bak"
run_train "$dir" locksource
expect_rc 2 "a registry dependency moving in the lock is uncommitted work"
expect_out "working tree is not clean" "registry lock change refuses as a dirty tree"
expect_no_out "restored Cargo.lock" "registry lock change is never restored"

# --- refusal: a source-less version bump is not drift if it is a registry crate's twin
dir="$(new_fixture locktwin)"
add_cargo_lock "$dir/work"
add_train_commit "$dir/work" "locktwin"
# Bump the GIT copy of `twin` (the block that carries `source =`), which a
# name-only match would wrongly accept as path drift.
sed -i.bak -e 's/^version = "0.2.1"$/version = "0.2.9"/' "$dir/work/Cargo.lock"
rm -f "$dir/work/Cargo.lock.bak"
run_train "$dir" locktwin
expect_rc 2 "bumping the sourced twin of a path crate refuses"
expect_out "working tree is not clean" "sourced twin bump refuses as a dirty tree"

# --- refusal: lock drift beside any other dirty file -----------------------
dir="$(new_fixture lockplus)"
add_cargo_lock "$dir/work"
add_train_commit "$dir/work" "lockplus"
sed -i.bak -e 's/^version = "0.17.45"$/version = "0.17.48"/' "$dir/work/Cargo.lock"
rm -f "$dir/work/Cargo.lock.bak"
echo "uncommitted" > "$dir/work/base.txt"
run_train "$dir" lockplus
expect_rc 2 "lock drift beside another dirty file refuses"
expect_out "working tree is not clean" "other dirty file still refuses"
expect_no_out "restored Cargo.lock" "the lock is not restored when the tree has other work"

# --- mutation: drop the source-less requirement and the twin arm goes green -
# Rewrite the drift check so a version bump on ANY block with that name and
# version counts, sourced or not, and run the sourced-twin input against the
# mutant. It must now (wrongly) restore. If it still refused, the twin arm
# above would not be testing the source-less predicate at all.
# The mutant is staged in a copy of the scripts directory so its relative
# `lib/` and sibling-script lookups resolve exactly as the original's do.
mutant_dir="$TMP_ROOT/mutant-scripts"
cp -R "$SCRIPT_DIR" "$mutant_dir"
mutant="$mutant_dir/train-push.sh"
sed 's/exit !(found \&\& !bad)/exit !(found)/' "$TRAIN_PUSH" > "$mutant"
chmod +x "$mutant"
grep -q 'exit !(found)' "$mutant" || fail "mutation did not apply; the awk exit line moved"
dir="$(new_fixture lockmutant)"
add_cargo_lock "$dir/work"
add_train_commit "$dir/work" "lockmutant"
sed -i.bak -e 's/^version = "0.2.1"$/version = "0.2.9"/' "$dir/work/Cargo.lock"
rm -f "$dir/work/Cargo.lock.bak"
TRAIN_PUSH_SAVED="$TRAIN_PUSH"; TRAIN_PUSH="$mutant"
run_train "$dir" lockmutant
TRAIN_PUSH="$TRAIN_PUSH_SAVED"
expect_out "restored Cargo.lock" "the mutant without the source-less check wrongly restores a sourced twin bump (proves the twin arm bites)"
expect_no_out "working tree is not clean" "the mutant does not refuse where the real predicate does"

# --- refusal: unresolved git operation -------------------------------------
dir="$(new_fixture midop)"
git -C "$dir/work" rev-parse HEAD > "$dir/work/.git/MERGE_HEAD"
run_train "$dir" midop
expect_rc 2 "MERGE_HEAD refuses"
expect_out "MERGE_HEAD present" "MERGE_HEAD names the reason"

# A rebase in progress is the directory; refuse on it.
dir="$(new_fixture midrebase)"
add_train_commit "$dir/work" "rebasing"
mkdir -p "$dir/work/.git/rebase-merge"
run_train "$dir" midrebase
expect_rc 2 "rebase-merge/ refuses"
expect_out "rebase-merge/ present" "an in-progress rebase names the directory"

# A bare REBASE_HEAD with no rebase directory is a finished rebase's leftover
# ref (git does not always remove it); it must be noted, not refused.
dir="$(new_fixture fossil)"
add_train_commit "$dir/work" "fossil"
git -C "$dir/work" rev-parse HEAD > "$dir/work/.git/REBASE_HEAD"
run_train "$dir" fossil
expect_rc 0 "a REBASE_HEAD fossil with no rebase directory does not refuse"
expect_out "REBASE_HEAD present with no rebase in progress" "the fossil is named on the header"

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

# --- local path-dependency preflight refuses an external Cargo path ----------
dir="$(new_fixture external-path-dependency)"
mkdir -p "$dir/work/src" "$dir/outside/src"
cat > "$dir/work/Cargo.toml" <<'TOML'
[package]
name = "fixture-root"
version = "0.1.0"
edition = "2021"

[dependencies]
escape = { path = "../outside" }
TOML
cat > "$dir/work/src/lib.rs" <<'RUST'
pub fn fixture() {}
RUST
cat > "$dir/outside/Cargo.toml" <<'TOML'
[package]
name = "escape"
version = "0.1.0"
edition = "2021"
TOML
cat > "$dir/outside/src/lib.rs" <<'RUST'
pub fn fixture() {}
RUST
cargo metadata --format-version 1 --offline --manifest-path "$dir/work/Cargo.toml" > /dev/null
git -C "$dir/work" add Cargo.toml Cargo.lock src/lib.rs
git -C "$dir/work" commit -qm "add external dependency path"
git -C "$dir/work" push -q origin "$DEFAULT_BRANCH"
add_train_commit "$dir/work" "external-path-dependency"
run_train "$dir" external-path-dependency
expect_rc 2 "external path dependency refuses before pushing"
expect_out "package=escape" "the local gate names the escaping package"
[ -z "$(origin_ref "$dir" refs/heads/train/external-path-dependency)" ] ||
  fail "the external path dependency preflight pushed a train branch"

# --- green: lands on main, train branch cleaned up -------------------------
dir="$(new_fixture green)"
add_train_commit "$dir/work" "green"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
: > "$dir/ci-state/capture_heartbeat"
run_train "$dir" green
expect_rc 0 "green CI lands"
[ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$train_sha" ] ||
  fail "green CI did not fast-forward origin/main to the tested sha"
[ -z "$(origin_ref "$dir" refs/heads/train/green)" ] ||
  fail "green CI left the train branch behind"
if grep -q '^pid=[0-9][0-9]*$' "$dir/ci-state/heartbeat-snapshot" &&
  grep -q '^start=.' "$dir/ci-state/heartbeat-snapshot" &&
  grep -q '^updated=.' "$dir/ci-state/heartbeat-snapshot"; then
  ok "the watcher heartbeat records pid, process start time, and poll time"
else
  fail "the watcher heartbeat did not record all liveness fields"
fi
[ ! -e "$dir/work/.git/train-push-green.watch" ] ||
  fail "a clean watcher exit left its heartbeat behind"

# A completed run whose verdict queries fail must not land. gh answers a
# network, rate-limit or auth error with empty output; reading that as "no
# failed jobs" fast-forwarded main on an unverified sha.
dir="$(new_fixture verdict-query-fails)"
add_train_commit "$dir/work" "verdict-query-fails"
base_sha="$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
echo "failure" > "$dir/ci-state/conclusion"
printf 'conclusion\njobs\n' > "$dir/ci-state/fail_json"
WATCH_CI_VERDICT_RETRY_SLEEP=0 run_train "$dir" verdict-query-fails
expect_rc 2 "a failed verdict query refuses to land"
expect_out "CI_UNDETERMINED" "a failed verdict query names the undetermined verdict"
[ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$base_sha" ] ||
  fail "a failed verdict query fast-forwarded main"

# A completed run that lists no jobs is not a run in which every job passed.
dir="$(new_fixture verdict-no-jobs)"
add_train_commit "$dir/work" "verdict-no-jobs"
base_sha="$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
echo "failure" > "$dir/ci-state/conclusion"
echo 0 > "$dir/ci-state/job_count"
WATCH_CI_VERDICT_RETRY_SLEEP=0 run_train "$dir" verdict-no-jobs
expect_rc 2 "an empty job list refuses to land"
[ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$base_sha" ] ||
  fail "an empty job list fast-forwarded main"

# A completed push run for the same commit on another branch is not proof that
# the train push started a run. The branch-qualified watch must fail plainly
# rather than treating that stale result as the train's CI.
dir="$(new_fixture no-train-run)"
add_train_commit "$dir/work" "no-train-run"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
echo "$DEFAULT_BRANCH" > "$dir/ci-state/run_branch"
run_train "$dir" norun
expect_rc 2 "a stale run for the sha on another branch does not authorize landing"
expect_out "no tests.yml run (event=push) appeared for $train_sha" \
  "missing train run is stated plainly"
expect_no_out "CI_DONE" "a completed run from another branch is never reported as this push result"
[ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" != "$train_sha" ] ||
  fail "a stale run from another branch landed the train sha"

# A signalled watch kills its timer child and removes its heartbeat rather than
# leaving descendants that make the watcher look alive.
dir="$(new_fixture watch-signal)"
echo "in_progress" > "$dir/ci-state/watch_status"
heartbeat="$dir/work/.git/train-push-watch-signal.watch"
(
  cd "$dir/work"
  exec env PATH="$BIN_DIR:$PATH" REPO=example/repo \
    OPERATOR_GH_FALLBACK_PATHS="$TMP_ROOT/no-such-fallback" \
    TRAIN_PUSH_TEST_STATE="$dir/ci-state" WATCH_CI_HEARTBEAT="$heartbeat" \
    WATCH_CI_POLL_SLEEP=60 scripts/watch-ci.sh 4242 > "$dir/watch-signal.out" 2>&1
) &
watcher_pid=$!
sleep_child=""
for _ in $(seq 1 100); do
  sleep_child="$(pgrep -P "$watcher_pid" -x sleep 2>/dev/null | head -1 || true)"
  [ -n "$sleep_child" ] && break
  sleep 0.02
done
if [ -z "$sleep_child" ]; then
  fail "the signalled-watch fixture never reached its timer sleep"
else
  kill -TERM "$watcher_pid" 2>/dev/null || true
  set +e
  wait "$watcher_pid"
  watcher_rc=$?
  set -e
  [ "$watcher_rc" -eq 143 ] || fail "the signalled watch exited $watcher_rc instead of 143"
  if kill -0 "$sleep_child" 2>/dev/null; then
    fail "the signalled watch left sleep child $sleep_child running"
    kill "$sleep_child" 2>/dev/null || true
  else
    ok "a signalled watch leaves no sleep child"
  fi
fi
[ ! -e "$heartbeat" ] || fail "a signalled watch left its heartbeat behind"

# --- the directory the process stands in may disappear during the watch ----
dir="$(new_fixture deleted-cwd)"
add_train_commit "$dir/work" "deleted-cwd"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
standing="$dir/work/standing"
mkdir -p "$standing"
cat > "$dir/ci-state/on-watch.sh" <<HOOK
#!/usr/bin/env bash
set -euo pipefail
rm -rf "$standing"
HOOK
chmod +x "$dir/ci-state/on-watch.sh"
TRAIN_PUSH_TEST_CWD="$standing" run_train "$dir" deleted-cwd
expect_rc 0 "a train lands after its starting directory is removed during the watch"
expect_no_out "Unable to read current working directory" "landing does not ask git to rediscover a deleted cwd"
[ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$train_sha" ] ||
  fail "the deleted-cwd train did not land its verified sha"

# Mutation control: remove the explicit checkout path from one post-watch git
# command; after the current directory is deleted, Git must fail to read it.
mutant_dir="$TMP_ROOT/cwd-mutant-scripts"
cp -R "$SCRIPT_DIR" "$mutant_dir"
mutant="$mutant_dir/train-push.sh"
sed 's/fetch_remote -q "$remote" "$default_branch"/git fetch -q "$remote" "$default_branch"/g' "$TRAIN_PUSH" > "$mutant"
chmod +x "$mutant"
grep -q '^  git fetch -q "$remote" "$default_branch"' "$mutant" ||
  fail "cwd mutation did not restore a bare post-watch git call"
dir="$(new_fixture deleted-cwd-mutant)"
add_train_commit "$dir/work" "deleted-cwd-mutant"
standing="$dir/work/standing"
mkdir -p "$standing"
cat > "$dir/ci-state/on-watch.sh" <<HOOK
#!/usr/bin/env bash
set -euo pipefail
rm -rf "$standing"
HOOK
chmod +x "$dir/ci-state/on-watch.sh"
TRAIN_PUSH_SAVED="$TRAIN_PUSH"; TRAIN_PUSH="$mutant"
TRAIN_PUSH_TEST_CWD="$standing" run_train "$dir" deleted-cwd-mutant
TRAIN_PUSH="$TRAIN_PUSH_SAVED"
expect_out "fatal: Unable to read current working directory: No such file or directory" \
  "a bare post-watch git call reproduces the deleted-cwd fatal (proves the cwd arm bites)"

# --- repository deletion after CI succeeds preserves a recoverable result ----
dir="$(new_fixture removed-repo)"
add_train_commit "$dir/work" "removed-repo"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
base_sha="$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
cat > "$dir/ci-state/on-watch.sh" <<HOOK
#!/usr/bin/env bash
set -euo pipefail
rm -rf "$dir/work"
HOOK
chmod +x "$dir/ci-state/on-watch.sh"
run_train "$dir" removed-repo
expect_rc 4 "a repository removed after green exits in the landing-failed state"
expect_out "repository path" "a removed repository root is identified distinctly"
expect_out "no longer exists" "a removed repository root says what disappeared"
expect_out "green run: https://github.com/example/repo/actions/runs/4242" \
  "removed-repository failure preserves the green run"
expect_out "train sha: $train_sha" "removed-repository failure preserves the verified sha"
expect_out "scripts/train-push.sh removed-repo --land" \
  "removed-repository failure gives the no-retest recovery command"
[ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$base_sha" ] ||
  fail "the removed-repository case moved the default branch"

# --- existing train: recorded green lands without a push or a watch --------
dir="$(new_fixture existing-green)"
add_train_commit "$dir/work" "existing-green"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
git -C "$dir/work" push -q origin "HEAD:refs/heads/train/existing-green"
: > "$dir/ci-state/gh-calls"
run_train "$dir" existing-green --land
expect_rc 0 "--land finishes an existing green train"
expect_out "landed previously-verified sha $train_sha" "the recovery names the exact CI-verified sha"
expect_no_out "creating origin/train/existing-green" "recorded green skips creating the train branch"
expect_no_out "updating origin/train/existing-green" "recorded green skips updating the train branch"
expect_no_out "watching CI" "recorded green skips the watch"
if grep -q -- '--json status,conclusion,jobs,url' "$dir/ci-state/gh-calls"; then
  fail "recorded green invoked watch-ci instead of spending the verdict"
else
  ok "recorded green did not invoke watch-ci"
fi
[ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$train_sha" ] ||
  fail "--land did not fast-forward the default branch to the recorded green sha"
[ -z "$(origin_ref "$dir" refs/heads/train/existing-green)" ] ||
  fail "--land left the recovered train branch behind"
if [ -s "$dir/ci-state/reruns" ]; then
  fail "an existing successful sha was rerun instead of landed"
else
  ok "an existing successful sha lands without a rerun"
fi

# --- recovery reports a dead watcher before updating the default branch -----
dir="$(new_fixture stale-heartbeat)"
add_train_commit "$dir/work" "stale-heartbeat"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
git -C "$dir/work" push -q origin "HEAD:refs/heads/train/stale-heartbeat"
cat > "$dir/work/.git/train-push-stale-heartbeat.watch" <<HEARTBEAT
pid=2147483000
start=Mon Jan  1 00:00:00 2001
updated=2026-09-19T20:15:00Z
HEARTBEAT
run_train "$dir" stale-heartbeat --land
expect_rc 0 "--land recovers a green sha from a dead watcher"
expect_out "watcher 2147483000 died at 2026-09-19T20:15:00Z; landing its verified sha $train_sha" \
  "recovery identifies the dead watcher and its last heartbeat"

# A live pid with a different process start is recycled, not a live watcher.
dir="$(new_fixture recycled-heartbeat)"
add_train_commit "$dir/work" "recycled-heartbeat"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
git -C "$dir/work" push -q origin "HEAD:refs/heads/train/recycled-heartbeat"
cat > "$dir/work/.git/train-push-recycled-heartbeat.watch" <<HEARTBEAT
pid=$$
start=not-the-current-process-start
updated=2026-09-19T20:16:00Z
HEARTBEAT
run_train "$dir" recycled-heartbeat --land
expect_rc 0 "a recycled heartbeat pid does not masquerade as a live watcher"
expect_out "watcher $$ died at 2026-09-19T20:16:00Z; landing its verified sha $train_sha" \
  "watcher liveness compares pid and process start time"

# The same pid and start time is a genuinely live watcher and must not be
# duplicated by a second process.
dir="$(new_fixture live-heartbeat)"
add_train_commit "$dir/work" "live-heartbeat"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
git -C "$dir/work" push -q origin "HEAD:refs/heads/train/live-heartbeat"
live_start="$(ps -p "$$" -o lstart= | awk '{$1=$1; print}')"
cat > "$dir/work/.git/train-push-live-heartbeat.watch" <<HEARTBEAT
pid=$$
start=$live_start
updated=2026-09-19T20:17:00Z
HEARTBEAT
run_train "$dir" live-heartbeat --land
expect_rc 2 "a matching heartbeat pid and process start refuses a duplicate watcher"
expect_out "watcher $$ is still running" "live watcher refusal names its pid"

# --- existing train: running attaches to its exact run ---------------------
dir="$(new_fixture existing-running)"
add_train_commit "$dir/work" "existing-running"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
git -C "$dir/work" push -q origin "HEAD:refs/heads/train/existing-running"
echo "in_progress" > "$dir/ci-state/recorded_status"
: > "$dir/ci-state/gh-calls"
run_train "$dir" existing-running
expect_rc 0 "an existing running train is watched and landed"
expect_out "attaching to CI for existing origin/train/existing-running at $train_sha" \
  "the running recovery attaches to the exact train tip"
if grep -q -- '--json status,conclusion,jobs,url' "$dir/ci-state/gh-calls"; then
  ok "running recovery invoked watch-ci"
else
  fail "running recovery landed without attaching watch-ci"
fi

# --- existing train: recorded red keeps the fix-and-repush contract --------
dir="$(new_fixture existing-red)"
add_train_commit "$dir/work" "existing-red"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
git -C "$dir/work" push -q origin "HEAD:refs/heads/train/existing-red"
echo "failure" > "$dir/ci-state/conclusion"
echo "Unit / failed job|9101" > "$dir/ci-state/failed_job"
echo "failure|9101" > "$dir/ci-state/rerun_jobs"
echo "failure" > "$dir/ci-state/rerun_conclusion"
run_train "$dir" existing-red
expect_rc 1 "an existing red train refuses to land"
expect_out "sha already ran in https://github.com/example/repo/actions/runs/4242 (failure); rerunning its failed and cancelled jobs (attempt 2)" \
  "an existing failed sha announces the rerun and new attempt"
expect_out "fix, commit, and re-run: scripts/train-push.sh existing-red" \
  "recorded red preserves the fix-and-repush instruction"
[ "$(cat "$dir/ci-state/reruns")" = "bypass=operator run rerun 4242 --failed" ] ||
  fail "an existing failed run was not rerun through the operator gh bypass"
grep -q -- '--attempt 2' "$dir/ci-state/gh-calls" ||
  fail "an existing failed run was not watched at attempt 2"
[ "$(origin_ref "$dir" refs/heads/train/existing-red)" = "$train_sha" ] ||
  fail "existing red moved or deleted the train branch"

# A failed old attempt is not the result of this push. Rerunning the failed
# jobs must produce a higher attempt, and only that attempt's green result lands.
dir="$(new_fixture existing-failed-rerun-green)"
add_train_commit "$dir/work" "existing-failed-rerun-green"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
git -C "$dir/work" push -q origin "HEAD:refs/heads/train/existing-failed-rerun-green"
echo "failure" > "$dir/ci-state/conclusion"
echo "Unit / runner unavailable|9103" > "$dir/ci-state/failed_job"
echo "failure|9103" > "$dir/ci-state/rerun_jobs"
echo "success" > "$dir/ci-state/rerun_conclusion"
run_train "$dir" existing-failed-rerun-green
expect_rc 0 "a failed existing run reruns and lands when the new attempt passes"
expect_out "(failure); rerunning its failed and cancelled jobs (attempt 2)" \
  "failed recovery identifies the new attempt"
expect_out "CI_DONE run=4242 conclusion=success attempt=2" \
  "recovery reports the new attempt's success"
[ "$(cat "$dir/ci-state/reruns")" = "bypass=operator run rerun 4242 --failed" ] ||
  fail "the failed existing run did not request a failed-jobs rerun"
grep -q -- '--attempt 2' "$dir/ci-state/gh-calls" ||
  fail "the successful retry was not watched at attempt 2"
[ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$train_sha" ] ||
  fail "the successful retry did not land the original verified sha"

# A run-level cancellation can leave individual jobs cancelled. `--failed`
# does not select those jobs, so recovery must request each cancelled job by id.
dir="$(new_fixture existing-cancelled-job)"
add_train_commit "$dir/work" "existing-cancelled-job"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
git -C "$dir/work" push -q origin "HEAD:refs/heads/train/existing-cancelled-job"
echo "cancelled" > "$dir/ci-state/conclusion"
printf 'cancelled|9102\ncancelled|9104\n' > "$dir/ci-state/rerun_jobs"
echo "success" > "$dir/ci-state/rerun_conclusion"
: > "$dir/ci-state/failed_job"
run_train "$dir" existing-cancelled-job
expect_rc 0 "a cancelled job is rerun and its new attempt lands"
expect_out "(cancelled); rerunning its failed and cancelled jobs (attempt 3)" \
  "cancelled recovery announces the new attempt"
[ "$(cat "$dir/ci-state/reruns")" = "bypass=operator run rerun 4242 --job 9102
bypass=operator run rerun 4242 --job 9104" ] ||
  fail "cancelled jobs were not rerun by job id through the operator gh bypass"
grep -q -- '--attempt 3' "$dir/ci-state/gh-calls" ||
  fail "the cancelled-job recovery did not watch attempt 3"
[ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$train_sha" ] ||
  fail "the cancelled-job recovery did not land the rerun sha"

# --- existing green is not landable after the default branch diverges ------
dir="$(new_fixture existing-diverged)"
add_train_commit "$dir/work" "existing-diverged"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
git -C "$dir/work" push -q origin "HEAD:refs/heads/train/existing-diverged"
advance_origin_main "$dir" "peer-landed-first"
advanced_sha="$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")"
run_train "$dir" existing-diverged --land
expect_rc 2 "--land refuses a green sha that is no longer a fast-forward"
expect_out "not a fast-forward" "the divergent recovery names the sound refusal"
expect_out "rebase and re-run" "the divergent recovery names the remedy without guessing why"
expect_no_out "main moved" "the divergent recovery does not assert why it is not a fast-forward"
[ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$advanced_sha" ] ||
  fail "the non-fast-forward recovery moved the default branch"

# Mutation control: ignoring the recorded green verdict must enter watch-ci.
mutant_dir="$TMP_ROOT/verdict-mutant-scripts"
cp -R "$SCRIPT_DIR" "$mutant_dir"
mutant="$mutant_dir/train-push.sh"
sed 's/\[ "$run_status" != "completed" \]/[ 1 -eq 1 ]/' "$TRAIN_PUSH" > "$mutant"
chmod +x "$mutant"
grep -q '\[ 1 -eq 1 \]' "$mutant" || fail "recorded-verdict mutation did not apply"
dir="$(new_fixture existing-green-mutant)"
add_train_commit "$dir/work" "existing-green-mutant"
git -C "$dir/work" push -q origin "HEAD:refs/heads/train/existing-green-mutant"
: > "$dir/ci-state/gh-calls"
TRAIN_PUSH_SAVED="$TRAIN_PUSH"; TRAIN_PUSH="$mutant"
run_train "$dir" existing-green-mutant --land
TRAIN_PUSH="$TRAIN_PUSH_SAVED"
if grep -q -- '--json status,conclusion,jobs,url' "$dir/ci-state/gh-calls"; then
  ok "ignoring the recorded verdict wrongly invokes watch-ci (proves the green recovery arm bites)"
else
  fail "the recorded-verdict mutant did not invoke watch-ci"
fi

# Mutation control: replacing the non-fast-forward refusal with a force push
# must move the default branch backwards, which the real negative arm forbids.
mutant_dir="$TMP_ROOT/force-mutant-scripts"
cp -R "$SCRIPT_DIR" "$mutant_dir"
mutant="$mutant_dir/train-push.sh"
sed 's|    refuse_non_fast_forward "$sha"|    git push --force-with-lease "$remote" "$sha:refs/heads/$default_branch"|' "$TRAIN_PUSH" > "$mutant"
chmod +x "$mutant"
grep -q 'git push --force-with-lease "$remote" "$sha:refs/heads/$default_branch"' "$mutant" ||
  fail "force-push mutation did not apply"
dir="$(new_fixture existing-diverged-mutant)"
add_train_commit "$dir/work" "existing-diverged-mutant"
train_sha="$(git -C "$dir/work" rev-parse HEAD)"
git -C "$dir/work" push -q origin "HEAD:refs/heads/train/existing-diverged-mutant"
advance_origin_main "$dir" "peer-landed-before-mutant"
TRAIN_PUSH_SAVED="$TRAIN_PUSH"; TRAIN_PUSH="$mutant"
run_train "$dir" existing-diverged-mutant --land
TRAIN_PUSH="$TRAIN_PUSH_SAVED"
if [ "$(origin_ref "$dir" "refs/heads/$DEFAULT_BRANCH")" = "$train_sha" ]; then
  ok "force-push mutant moves the default branch backwards (proves the negative arm bites)"
else
  fail "force-push mutant did not violate the negative arm"
fi

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

# --- a train carrying a merge is never auto-rebased -------------------------
# A plain rebase drops the merge commit and anything recorded only in it (a
# conflict resolution, an integration fix), exits 0, and lands the reduced
# tree. BROCA's specimen: the merge-only file vanished silently. The script
# must refuse the requeue and leave the remote train ref intact.
dir="$(new_fixture mergetrain)"
git -C "$dir/work" checkout -qb side
echo "side" > "$dir/work/side.txt"
git -C "$dir/work" add side.txt
git -C "$dir/work" commit -qm "side: add side.txt"
git -C "$dir/work" checkout -q "$DEFAULT_BRANCH"
echo "main edit" > "$dir/work/main-edit.txt"
git -C "$dir/work" add main-edit.txt
git -C "$dir/work" commit -qm "main: edit"
git -C "$dir/work" merge -q --no-ff side -m "merge side"
# The integration fix lives only in the merge commit.
echo "integration" > "$dir/work/integration.txt"
git -C "$dir/work" add integration.txt
git -C "$dir/work" commit -q --amend --no-edit
merge_sha="$(git -C "$dir/work" rev-parse HEAD)"
cat > "$dir/ci-state/on-watch.sh" <<HOOK
#!/usr/bin/env bash
set -euo pipefail
export HOME="$HOME"
DEFAULT_BRANCH="$DEFAULT_BRANCH"
$(declare -f advance_origin_main)
advance_origin_main "$dir" "peer moved main"
HOOK
chmod +x "$dir/ci-state/on-watch.sh"
run_train "$dir" mergetrain
expect_rc 3 "a merge-carrying train refuses the automatic rebase"
expect_out "not rebasing" "merge refusal says it is not rebasing"
expect_out "merge $merge_sha" "merge refusal names the merge commit"
[ "$(git -C "$dir/work" rev-parse HEAD)" = "$merge_sha" ] ||
  fail "merge refusal moved HEAD"
[ -f "$dir/work/integration.txt" ] ||
  fail "merge refusal lost the merge-only file"
[ "$(origin_ref "$dir" refs/heads/train/mergetrain)" = "$merge_sha" ] ||
  fail "merge refusal did not leave the remote train ref intact"

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
expect_out "no tests.yml run (event=push) appeared" "stale green waited for the rebased sha's run"
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
expect_rc 4 "a protection refusal after green exits in the landing-failed state"
expect_out "refused: $protected_sha has no status check on origin. Merge onto the train branch and push there; CI runs on the merge sha, then main fast-forwards." \
  "protection refusal prints the merge-as-a-train remedy"
expect_out "green run: https://github.com/example/repo/actions/runs/4242" \
  "post-CI failure names the green run"
expect_out "train sha: $protected_sha" "post-CI failure names the verified train sha"
expect_out "scripts/train-push.sh protected --land" \
  "post-CI failure gives the no-retest recovery command"
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

# --- the gating workflow is a parameter, not this repository's filename ----
# Lifts whose gate is ci.yml refused at "no .github/workflows/tests.yml" while
# WATCH_CI_WORKFLOW=ci.yml was set: the scan read a hardcoded path and the
# probe read the variable. One variable now feeds both.
dir="$(new_fixture ciyml)"
git -C "$dir/work" mv .github/workflows/tests.yml .github/workflows/ci.yml
sed -i.bak 's/^name: Tests$/name: CI/' "$dir/work/.github/workflows/ci.yml" && rm -f "$dir/work/.github/workflows/ci.yml.bak"
git -C "$dir/work" add -A .github/workflows
git -C "$dir/work" commit -qm "gate is ci.yml"
echo "CI" > "$dir/ci-state/workflow_name"
add_train_commit "$dir/work" "ciyml"
run_train "$dir" ciyml
expect_rc 2 "without WATCH_CI_WORKFLOW a ci.yml repository refuses"
expect_out "set WATCH_CI_WORKFLOW=" "the refusal names the override"
expect_out "ci.yml" "the refusal lists the workflow files present"
WATCH_CI_WORKFLOW=ci.yml run_train "$dir" ciyml
expect_rc 0 "with WATCH_CI_WORKFLOW=ci.yml the same repository lands"

# --- first-run probe finds its run by sha + display name, not by file ------
# A train that RENAMES the gating workflow file: the forge cannot list the new
# file until it lands, so a file-filtered run query finds nothing for a probe
# whose run started. The display name survives the rename.
dir="$(new_fixture renamed)"
rm -f "$dir/work/.git/train-push-proven"
git -C "$dir/work" mv .github/workflows/tests.yml .github/workflows/gate.yml
git -C "$dir/work" add -A .github/workflows
git -C "$dir/work" commit -qm "rename the gate file"
add_train_commit "$dir/work" "renamed"
WATCH_CI_WORKFLOW=gate.yml run_train "$dir" renamed
expect_rc 0 "a train that renames the gating workflow still proves its trigger"
expect_out "trigger proven" "the probe found the run under the workflow's display name"

# --- repository slug: derived from origin, refused when underivable ---------
dir="$(new_fixture slug)"
add_train_commit "$dir/work" "slug"
TRAIN_PUSH_TEST_REPO="" run_train "$dir" slug
expect_rc 2 "a local-path origin cannot yield a repository slug and refuses"
expect_out "set REPO=owner/name" "the refusal names the override"
# A github-shaped configured URL, rewritten to the local bare repo for the
# actual fetch/push: the slug is read from the configured value, which is what
# an operator's clone carries; insteadOf keeps the fixture offline.
git -C "$dir/work" remote set-url origin "git@github.com:example/derived.git"
git -C "$dir/work" config url."$dir/origin.git".insteadOf "git@github.com:example/derived.git"
TRAIN_PUSH_TEST_REPO="" run_train "$dir" slug
expect_rc 0 "a github-shaped origin lands without REPO"
expect_out "example/derived" "the slug is derived from origin's configured URL"

# --- concurrent trains: mkdir-atomic lock; a live owner refuses, a stale owner
# is replaced, a leftover ref lists its delete and proceeds, the lock is taken
# before any ref moves, and two trains started together cannot both be admitted
# (names mirror BROCA's suite so the two copies stay comparable) --------------
lock_held() { printf '%s/work/.git/train-push-locks/held' "$1"; }
plant_owner() { mkdir -p "$(lock_held "$1")"; printf '%s %s\n' "$2" "$3" > "$(lock_held "$1")/owner"; }
# plant_owner_started DIR PID NAME START: an owner recorded with a start time.
plant_owner_started() { mkdir -p "$(lock_held "$1")"; printf '%s %s\n%s\n' "$2" "$3" "$4" > "$(lock_held "$1")/owner"; }
process_start() { ps -p "$1" -o lstart= 2>/dev/null | awk '{$1=$1; print}'; }
owner_line() { head -1 "$(lock_held "$1")/owner" 2>/dev/null; }
# test_clean_state_does_not_refuse_for_concurrency
dir="$(new_fixture pidlock-clean)"
add_train_commit "$dir/work" "clean"
run_train "$dir" clean
expect_rc 0 "a clean state lands without a concurrency refusal"
expect_no_out "is RUNNING as pid" "no train was reported running"
# test_the_lock_is_claimed_before_any_ref_moves: the owner must be recorded even
# on a run that never reached the push - a lock taken after the push would leave
# the resolve-HEAD-to-push window uncovered, the hole the ref-based guard had.
dir="$(new_fixture lock-order)"
add_train_commit "$dir/work" "lock-order"
echo "failure" > "$dir/ci-state/conclusion"
echo "Unit / broken" > "$dir/ci-state/failed_job"
run_train "$dir" lockorder
expect_rc 1 "the CI-red run stops before landing"
case "$(owner_line "$dir")" in
  *" lockorder") ok "this train owns the lock after a run that never landed" ;;
  *) fail "the lock was not claimed before the push" ;;
esac
# test_a_live_lock_refuses_and_names_the_pid
dir="$(new_fixture live-lock)"
add_train_commit "$dir/work" "live"
sleep 60 &
sleeper=$!
plant_owner_started "$dir" "$sleeper" other "$(process_start "$sleeper")"
run_train "$dir" live
kill "$sleeper" 2>/dev/null || true
expect_rc 2 "a live lock refuses the second train"
expect_out "train other is RUNNING as pid $sleeper" "the refusal names the train and its pid"
if [ -z "$(origin_ref "$dir" refs/heads/train/live)" ]; then
  ok "the refused train pushed no ref"
else
  fail "the refused train pushed a ref"
fi
# test_a_stale_lock_is_cleared_and_does_not_refuse: a dead owner is replaced by
# rename-claim, so this train's name ends up in the owner file.
dir="$(new_fixture stale-lock)"
add_train_commit "$dir/work" "stale"
plant_owner "$dir" 2147483000 gone
run_train "$dir" stale
expect_rc 0 "a stale lock (dead pid) does not refuse"
case "$(owner_line "$dir")" in
  *" gone") fail "the stale owner was obeyed instead of replaced" ;;
  *" stale") ok "the stale owner was replaced by this train" ;;
  *) fail "the owner file is neither the stale owner nor this train" ;;
esac
# test_a_stale_owner_recovered_by_another_train_is_not_dispossessed: two trains
# read the same stale owner; the other one recovers first and takes a fresh,
# live lock. This train must meet that live lock and refuse, never remove it.
dir="$(new_fixture stale-race)"
add_train_commit "$dir/work" "stale-race"
plant_owner "$dir" 2147483000 gone
sleep 60 &
winner=$!
winner_start="$(process_start "$winner")"
cat > "$dir/race-hook.sh" <<HOOK
#!/usr/bin/env bash
held="$(lock_held "$dir")"
rm -rf "\$held"
mkdir "\$held"
printf '%s %s\n%s\n' "$winner" winner "$winner_start" > "\$held/owner"
HOOK
chmod +x "$dir/race-hook.sh"
TRAIN_PUSH_TEST_LOCK_RACE_HOOK="$dir/race-hook.sh" run_train "$dir" stale-race
kill "$winner" 2>/dev/null || true
expect_rc 2 "a train that lost the stale-lock recovery race refuses"
case "$(owner_line "$dir")" in
  *" winner") ok "the winner's fresh lock survived the losing recoverer" ;;
  *) fail "the losing recoverer took the winner's fresh lock" ;;
esac
[ -z "$(origin_ref "$dir" refs/heads/train/stale-race)" ] ||
  fail "the losing recoverer pushed a train ref"
# test_a_recycled_pid_is_stale: the recorded pid is alive but is a different
# process (its start time differs), as when macOS reuses a finished train's pid.
dir="$(new_fixture recycled-pid)"
add_train_commit "$dir/work" "recycled"
sleep 60 &
recycled=$!
plant_owner_started "$dir" "$recycled" gone "Mon Jan  1 00:00:00 2001"
run_train "$dir" recycled
expect_rc 0 "a live pid with a different start time does not refuse"
case "$(owner_line "$dir")" in
  *" recycled") ok "the recycled pid's lock was replaced by this train" ;;
  *) fail "the recycled pid's lock was obeyed" ;;
esac
# test_a_legacy_owner_on_a_non_train_pid_is_stale: a lock written before start
# times were recorded, whose pid now runs something that is not a train.
dir="$(new_fixture legacy-owner)"
add_train_commit "$dir/work" "legacy"
plant_owner "$dir" "$recycled" gone
run_train "$dir" legacy
kill "$recycled" 2>/dev/null || true
expect_rc 0 "a legacy lock on a non-train pid does not refuse"
case "$(owner_line "$dir")" in
  *" legacy") ok "the legacy lock was replaced by this train" ;;
  *) fail "the legacy lock on a non-train pid was obeyed" ;;
esac
# an unreadable owner (killed between mkdir and the owner write) is refused,
# never dispossessed
dir="$(new_fixture unowned-lock)"
add_train_commit "$dir/work" "unowned"
mkdir -p "$(lock_held "$dir")"
run_train "$dir" unowned
expect_rc 2 "a lock with no readable owner refuses"
expect_out "rm -rf" "the refusal names the manual clear"
# test_a_leftover_ref_lists_its_delete_and_proceeds
dir="$(new_fixture leftover-ref)"
add_train_commit "$dir/work" "leftover"
git -C "$dir/work" push -q origin "HEAD:refs/heads/train/abandoned"
run_train "$dir" leftover
expect_rc 0 "a leftover ref with no live train proceeds"
expect_out "git push origin --delete train/abandoned" "the leftover ref's delete is composed"
expect_out "cannot race this train" "the run says why it proceeds"
# test_two_trains_started_together_cannot_both_be_admitted: against a local bare
# origin `ls-remote` is microseconds, so two processes never land in a window
# that genuinely exists; a git shim that makes ls-remote take 0.5 s widens the
# real race rather than inventing one. Exactly one name may own the lock and
# the other must have been refused.
dir="$(new_fixture race)"
add_train_commit "$dir/work" "race"
mkdir -p "$dir/fixturebin"
real_git="$(command -v git)"
printf '#!/bin/bash\nfor a in "$@"; do [ "$a" = "ls-remote" ] && sleep 0.5; done\nexec %s "$@"\n' "$real_git" > "$dir/fixturebin/git"
chmod +x "$dir/fixturebin/git"
(
  cd "$dir/work" && PATH="$dir/fixturebin:$BIN_DIR:$PATH" REPO=example/repo \
    OPERATOR_GH_FALLBACK_PATHS="$TMP_ROOT/no-such-fallback" TRAIN_PUSH_TEST_STATE="$dir/ci-state" \
    WATCH_CI_RESOLVE_ATTEMPTS=1 WATCH_CI_RESOLVE_SLEEP=0 TRAIN_PUSH_PROBE_ATTEMPTS=2 TRAIN_PUSH_PROBE_SLEEP=0 \
    "$TRAIN_PUSH" racer-a > "$dir/racer-a.out" 2>&1
) &
racer_a=$!
(
  cd "$dir/work" && PATH="$dir/fixturebin:$BIN_DIR:$PATH" REPO=example/repo \
    OPERATOR_GH_FALLBACK_PATHS="$TMP_ROOT/no-such-fallback" TRAIN_PUSH_TEST_STATE="$dir/ci-state" \
    WATCH_CI_RESOLVE_ATTEMPTS=1 WATCH_CI_RESOLVE_SLEEP=0 TRAIN_PUSH_PROBE_ATTEMPTS=2 TRAIN_PUSH_PROBE_SLEEP=0 \
    "$TRAIN_PUSH" racer-b > "$dir/racer-b.out" 2>&1
) &
racer_b=$!
wait "$racer_a" || true
wait "$racer_b" || true
race_owner="$(owner_line "$dir")"
case "$race_owner" in
  *" racer-a") race_loser="racer-b" ;;
  *" racer-b") race_loser="racer-a" ;;
  *) race_loser="" ;;
esac
if [ -n "$race_loser" ]; then
  ok "exactly one racer owns the lock (${race_owner#* })"
  if grep -q "a train is already running" "$dir/$race_loser.out"; then
    ok "the loser ($race_loser) was refused, not admitted"
  else
    fail "the loser ($race_loser) was admitted alongside the winner"
  fi
else
  fail "the lock owner is not a single racer: '$race_owner'"
fi

if [ "$failures" -ne 0 ]; then
  printf 'train-push.test.sh: %s check(s) failed\n' "$failures" >&2
  exit 1
fi
echo "train-push.test.sh: passed ($checks checks)"
