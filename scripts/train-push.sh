#!/usr/bin/env bash
# Push a train to its own branch, let CI be the gate, and advance main only on
# green. Operator tooling runs through the real GitHub CLI (gh) via watch-ci.sh;
# the shim is only for AI agent commands.
# Native Windows pre-push smoke: scripts/windows-gate.sh with runon "windows"
# (opt-in).
#
# Lifting this into another repository: carry five files together —
# scripts/train-push.sh, scripts/watch-ci.sh, scripts/check-path-deps.py,
# scripts/lib/operator-gh.sh, scripts/lib/workflow-gates.py — plus `python3`,
# `git`, and the real `gh` on PATH. check-path-deps.py may be replaced by the
# repository's own check under the same name. Nothing else here assumes this
# repository's layout: the default branch
# is read from origin/HEAD, and repo-local preflights run only when their
# scripts exist (see "Repo-local preflights" below), with the header line
# naming which ones ran.
#
# Why this is the default push path instead of scripts/gated-push.sh:
# the local full Rust gate takes ~12 minutes on a developer machine and only
# sees that machine's operating system.
# Measured over the last week, about half the red trains failed on Linux or
# Windows only, so on those the local gate was paid for and could not have
# caught the failure. It also runs in the one working tree, so trains serialize
# behind each other and compete with worker sessions for this box's CPU (hence
# the box gate, the nice steering, and peers asking for quiet windows). CI runs
# the three platforms in parallel on other hardware in ~11-16 minutes.
#
# The contract we keep is "main is never red". That is a property of what lands
# on main, not of where the tests ran: nothing lands here except a sha whose CI
# run concluded success, and it lands by fast-forward only - never a merge,
# never a force. gated-push.sh stays for the work whose failures only reproduce
# on a developer machine (its native file watcher, or its executable assessment).
#
# EDIT THIS FILE BY WRITE-TEMP-THEN-RENAME, NEVER IN PLACE. A running bash holds
# an open fd and reads on by byte offset, so an in-place write (same inode) makes
# a live train resume at the wrong offset and die on a syntax error in code it
# never reached - "line 789: syntax error near unexpected token `('" in a file
# that passes `bash -n` before and after. A rename into place (new inode) leaves
# the live run on the old bytes. Nothing lands from such a death: it happens before
# the landing push, and the ref left behind is the recoverable repush state.
#
# A MERGE IS ITSELF A TRAIN PUSH. Under required status checks, a merge commit
# made locally has no check of its own - main's protection sees an unchecked
# sha and refuses the push, however green both sides were separately. So do the
# merge on the train branch, let CI run on the merge sha, and fast-forward main
# to that same sha. A merge commit whose first parent is origin/main is already
# a descendant of it, so it fast-forwards like any other train.
#
# Usage:
#   scripts/train-push.sh <train-name>
#   scripts/train-push.sh <train-name> --on <parent-train>
#   scripts/train-push.sh <train-name> --land [--on <parent-train>]
#   scripts/train-push.sh <train-name> --restack [--on <parent-train>]
#   scripts/train-push.sh <train-name> -- <local smoke command...>
#
# The optional smoke is the targeted slice the diff touches (the one or two
# suites you would rerun by hand), NOT the full gate. It exists to catch an
# obvious break before spending a CI run.
#
# A red train leaves origin/train/<train-name> in place: fix, commit, and run
# this script again with the same name to update the branch and re-run CI.
#
# STACKED TRAINS START CI TOGETHER. Build each child on its parent's pushed
# train tip, in a separate worktree, and pass --on <parent-train>. Without --on,
# the nearest unlanded origin/train/* tip strictly below HEAD is detected (an
# ambiguous tip asks for --on). The child pushes immediately, watches its own
# run, then waits up to an hour for its exact parent base to appear on main.
# TRAIN_PUSH_PARENT_ATTEMPTS (360) and TRAIN_PUSH_PARENT_SLEEP (10 seconds) bound
# that wait. A green child never authorizes landing its still-unlanded parent.
# When the base is on main and main is an ancestor of the tested child sha, the
# child fast-forwards without a rebase or another CI run. This works at any depth.
#
# A red, changed, or deleted parent blocks the child, preserving its own CI
# result. Fix and repush the parent, then --restack the child: only the child's
# commits are rebased onto the current parent tip, or main if the parent ref has
# gone. It repushes and waits for fresh CI on the new sha. Merge-carrying children
# refuse automatic restacking for the same reason they refuse automatic requeue.
# The parent name and exact base are recorded atomically in the shared git dir;
# recovery (--land or same-sha repush) reads that record even after the parent
# branch is deleted. Restack from the same clone; other clones have no record of
# a vanished or rewritten parent and must reconstruct the stack by hand.
# One train per worktree and one process per train name are still enforced.
#
# Search-quality descriptors keep their existing CI gate: the class is derived
# from the merge-base with main, NOT from the parent tip. A stacked child must
# describe the whole pending diff (parents included) against main. The workflow
# already uses that base; stacking does not narrow it or waive descriptor checks.
#
# If main moves while an unstacked train's CI runs, it is re-queued: rebase onto
# the new origin/main, re-push the branch, watch a fresh run - up to 3 rounds.
# The re-queue is for a moved branch ONLY. A red run is never retried: the same
# tree on a new base is red for the same reason, and version/lock skew in
# particular ends in a lockfile bump commit rather than in any retry.
#
# THE INVARIANT THE RE-QUEUE KEEPS: only the sha a check actually ran against
# may fast-forward main. A green run for the pre-rebase sha proves something
# about a base that no longer exists, so it never authorizes the landing of the
# rebased commit; the rebased sha gets pushed and waits for its own run. That is
# also why a moved main cannot be resolved by pushing the old sha harder.
#
# RELEASES ARE TRAINS TOO. Under required checks a release tag must point at a
# sha that is already on the default branch by fast-forward and carries the
# green check, so the sequence is: train, fast-forward, tag the green sha. The
# local full gate drops out of the release scripts along with everything else -
# tagging a locally-gated sha that never went through a train produces a tag
# whose commit no check ever saw.
#
# WHERE BRANCH PROTECTION IS UNAVAILABLE (a private repo on a Free plan has no
# required status checks), this script's fast-forward-only-on-green IS the gate.
# Nothing on the server will stop a direct push then, so the discipline of
# pushing through here is the whole of the contract.
#
# Exit codes:
#   0  landed on the default branch
#   1  CI red (or a push that reported success without moving origin)
#   2  precondition refusal, bad usage, failed smoke, no CI run resolved, or a
#      blocked stack (red/changed/abandoned parent, or parent wait limit)
#   3  the default branch kept moving through 3 re-queue rounds, or the rebase
#      conflicted
#   4  CI is green, but landing failed; re-run with --land to finish without
#      paying for another CI run
#   5  a GitHub CLI query failed and was refused rather than treated as empty
set -euo pipefail

# Run from a private copy of this file. bash reads a script incrementally, so an
# edit to scripts/train-push.sh while a train is watching CI (a pick of a
# peer's fix, a rebase that moves the file) shifts the running instance's read
# offset and it dies at its next statement with a syntax error, after CI went
# green and before the fast-forward (train 121, 2026-09-19). The copy is
# immune; the instance remembers the real path for its own diagnostics.
if [ -z "${TRAIN_PUSH_EXEC_COPY:-}" ]; then
  train_push_copy="$(mktemp "${TMPDIR:-/tmp}/train-push.XXXXXX")"
  cp "${BASH_SOURCE[0]}" "$train_push_copy"
  TRAIN_PUSH_EXEC_COPY="$train_push_copy" \
    exec bash "$train_push_copy" "$@"
fi
trap 'rm -f "$TRAIN_PUSH_EXEC_COPY"' EXIT

repo_slug_override="${REPO:-}"
REPO="$(git rev-parse --show-toplevel)" || {
  printf 'train-push: refusing — cannot resolve the repository while the current working directory is valid\n' >&2
  exit 2
}
script_dir="$REPO/scripts"

remote="origin"
# Resolved from the remote below, never assumed: this repo's default branch is
# main today, but a script that hardcodes it silently lands trains on a branch
# that is not the one protection and CI are configured for.
default_branch=""
# The workflow whose run gates a landing, shared with watch-ci.sh so the probe
# and the watch cannot disagree about which run counts. WATCH_CI_WORKFLOW is
# the one override for both scripts (a lift with `ci.yml` sets it once); the
# step-0 scan below reads the same variable, so the file it checks and the run
# it waits for cannot name two different workflows.
tests_workflow_name="${WATCH_CI_WORKFLOW:-tests.yml}"
# The repository the runs live in: from origin unless REPO is given, the same
# derivation watch-ci.sh uses, so the probe and the watch query one repository.
# A fixed default here was kept by the first lift and watched this repository's
# runs for another repository's trains.
repo_from_origin() {
  local url
  url="$(git -C "$REPO" config --get "remote.$remote.url" 2>/dev/null)" || return 1
  case "$url" in
    git@github.com:*) url="${url#git@github.com:}" ;;
    https://github.com/*) url="${url#https://github.com/}" ;;
    ssh://git@github.com/*) url="${url#ssh://git@github.com/}" ;;
    *) return 1 ;;
  esac
  printf '%s\n' "${url%.git}"
}
repo_slug="$repo_slug_override"
# How long the first-run probe waits for a run to start. Overridable for the
# same reason watch-ci.sh's resolver knobs are: tests cannot wait out the
# real budget.
probe_attempts="${TRAIN_PUSH_PROBE_ATTEMPTS:-12}"
probe_sleep="${TRAIN_PUSH_PROBE_SLEEP:-10}"
# Failing job or step names that mean the red is dependency skew rather than
# a broken change: in this repo those are the Cargo.lock and manifest checks.
# Steps are matched too because a seat with single-job CI names its jobs after
# the platform, and the lock check is a step inside it. Extend the pattern
# when a repo names such a job or step something else.
skew_pattern="${TRAIN_PUSH_SKEW_PATTERN:-}"
if [ -z "$skew_pattern" ]; then
  skew_pattern='lock|version|pin|sibling'
fi

# A lockfile name next to a drift word in the TAIL of a failing job's log.
# A lock check that is one phase inside a larger step ends in such a line
# ("Cargo.lock drifted", "bun.lock is out of date"), and the tail is the only
# part of a failing log that can still name skew once the step name is
# generic. Only the tail is judged, so a Cargo.lock mention in an early build
# line cannot call a later test failure skew; and unlike the name arms this
# one stays narrower than skew_pattern on purpose, because matching real
# output is easier to keep conservative than matching prose.
skew_log_tail() {
  tail -40 | grep -Ei 'Cargo\.lock|bun\.lock' | grep -Eqi 'drift|stale|out of date|out-of-date|changed|would change|differs|mismatch'
}

# The whole skew verdict, from the failing jobs' name|job-id lines as the
# stubbed or real gh reports them. Job names are judged first (cheap, no extra
# queries); step names come from the jobs API and cover single-job CI, whose
# job names say only the platform. Advisory jobs are already excluded from the
# watch log the pairs are parsed from, the same exclusion watch-ci.sh applies,
# so a lock-flavoured step in a non-gating job cannot call a real red skew.
red_is_skew() {
  local run_id="$1"
  local failing_pairs="$2"
  local line job_id job_steps job_log log_rc
  if printf '%s\n' "$failing_pairs" | grep -Eqi "$skew_pattern"; then
    return 0
  fi
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    job_id="${line##*|}"
    job_steps="$(gh_query_or_refuse "failed steps for CI job $job_id" "$OPERATOR_GH" \
      api "repos/$repo_slug/actions/jobs/$job_id" \
      --jq '.steps[] | select(.conclusion=="failure") | .name')" || return "$?"
    if printf '%s\n' "$job_steps" | grep -Eqi "$skew_pattern"; then
      return 0
    fi
    # A log is advisory skew evidence, not the CI verdict. Give transient log
    # failures the same bounded retry as other reads, but keep a final read
    # failure from changing an ordinary red result into an unknown verdict.
    # GitHub job logs often contain ANSI color escapes, which gh must allow.
    if job_log="$(gh_query_or_refuse "log for CI run $run_id job $job_id" "$OPERATOR_GH" \
      api --allow-escape-sequences "repos/$repo_slug/actions/jobs/$job_id/logs")"; then
      if printf '%s\n' "$job_log" | skew_log_tail; then
        return 0
      fi
    else
      log_rc=$?
      [ "$log_rc" -eq 5 ] || return "$log_rc"
      printf 'train-push: skew check skipped for CI job %s: its log could not be read after bounded retries\n' \
        "$job_id" >&2
    fi
  done <<< "$failing_pairs"
  return 1
}

say() { printf 'train-push: %s\n' "$1"; }
refuse() {
  printf 'train-push: refusing — %s\n' "$1" >&2
  exit 2
}

# A failed GitHub query is not an empty result: empty run lists and unreadable
# conclusions have different safety meanings. Retry transient service and
# network errors a bounded number of times, then preserve the upstream error.
# GitHub sometimes answers HTTP 200 with a truncated body, which gh reports as
# "unexpected end of JSON input"; a well-formed reply never parses that way, so
# it is transient too. A real failed check is a well-formed reply and is never
# retried here.
gh_query_or_refuse() {
  local context="$1"
  shift
  local attempt=1 rc output diagnostic_file error_line error_text delay total_delay=0
  local transient_error
  local -a delays

  read -r -a delays <<< "${TRAIN_PUSH_GH_RETRY_DELAYS:-30 60 120 240}"
  if [ "${#delays[@]}" -ne 4 ]; then
    refuse "TRAIN_PUSH_GH_RETRY_DELAYS must contain four nonnegative integers"
  fi
  for delay in "${delays[@]}"; do
    [[ "$delay" =~ ^[0-9]+$ ]] ||
      refuse "TRAIN_PUSH_GH_RETRY_DELAYS must contain four nonnegative integers"
    total_delay=$((total_delay + delay))
  done
  [ "$total_delay" -le 600 ] ||
    refuse "TRAIN_PUSH_GH_RETRY_DELAYS must total no more than 600 seconds"

  transient_error='rate.?limit|secondary[[:space:]]+rate|abuse[[:space:]]+detection|http[[:space:]]+(429|5[0-9][0-9])|status[[:space:]]+code:[[:space:]]*(429|5[0-9][0-9])|network[[:space:]]+(is[[:space:]]+)?unreachable|temporary[[:space:]]+failure|could[[:space:]]+not[[:space:]]+(resolve|connect)|no[[:space:]]+such[[:space:]]+host|connection[[:space:]]+(refused|reset|closed|aborted|timed[[:space:]]+out)|timed[[:space:]]+out|timeout|context[[:space:]]+deadline[[:space:]]+exceeded|dial[[:space:]]+(tcp|udp)|failed[[:space:]]+to[[:space:]]+connect|tls[[:space:]]+handshake|unexpected[[:space:]]+eof|unexpected[[:space:]]+end[[:space:]]+of[[:space:]]+json[[:space:]]+input|i/o[[:space:]]+timeout|server[[:space:]]+misbehaving|remote[[:space:]]+end[[:space:]]+hung[[:space:]]+up|broken[[:space:]]+pipe|socket[[:space:]]+hang[[:space:]]+up|proxy.*(error|failed)'
  while :; do
    diagnostic_file="$(mktemp "${TMPDIR:-/tmp}/train-push-gh.XXXXXX")" || {
      printf 'train-push: could not query %s: could not create a diagnostic file\n' "$context" >&2
      return 5
    }
    if output="$("$@" 2>"$diagnostic_file")"; then
      [ ! -s "$diagnostic_file" ] || cat "$diagnostic_file" >&2
      rm -f "$diagnostic_file"
      printf '%s\n' "$output"
      return 0
    else
      rc=$?
    fi

    error_line="$(head -n 1 "$diagnostic_file" 2>/dev/null || true)"
    if [ -z "$error_line" ] && [ -n "$output" ]; then
      error_line="${output%%$'\n'*}"
    fi
    error_line="${error_line:-gh exited $rc without an error message}"
    error_text="$(cat "$diagnostic_file" 2>/dev/null || true)"
    [ -z "$output" ] || error_text="$error_text $output"
    error_text="$(printf '%s' "$error_text" | tr '[:upper:]' '[:lower:]')"
    rm -f "$diagnostic_file"

    if [[ "$error_text" =~ $transient_error ]] && [ "$attempt" -le "${#delays[@]}" ]; then
      delay="${delays[$((attempt - 1))]}"
      printf 'train-push: GitHub query for %s failed transiently; retrying (attempt %s of %s after %ss)\n' \
        "$context" "$((attempt + 1))" "$((${#delays[@]} + 1))" "$delay" >&2
      sleep "$delay" || {
        printf 'train-push: could not query %s: sleep before retry failed\n' "$context" >&2
        return 5
      }
      attempt=$((attempt + 1))
      continue
    fi
    printf 'train-push: could not query %s: %s\n' "$context" "$error_line" >&2
    return 5
  done
}

# Linked worktrees share refs/remotes even though their HEADs are independent.
# Git rejects a fetch if another fetch or push changes a tracking ref after
# its read. Only that compare-and-swap race or an existing ref lock is
# transient: auth, transport and permission failures still stop immediately.
fetch_remote() {
  local output rc attempt delay
  for attempt in 1 2 3 4 5; do
    if output="$(git -C "$REPO" fetch "$@" 2>&1)"; then
      [ -z "$output" ] || printf '%s\n' "$output" >&2
      return 0
    else
      rc=$?
    fi
    printf '%s\n' "$output" >&2
    if [[ "$output" == *"cannot lock ref 'refs/remotes/"* ]] &&
      { [[ "$output" == *" but expected "* ]] || [[ "$output" == *"File exists"* ]]; }; then
      [ "$attempt" -lt 5 ] || return "$rc"
      # Independent processes must not wake up in lockstep on the next try.
      printf -v delay '0.%03d' "$((50 + RANDOM % 151))"
      say "git fetch hit a shared ref update race; retrying (attempt $((attempt + 1)) of 5 after ${delay}s)"
      sleep "$delay" || return "$?"
    else
      return "$rc"
    fi
  done
}

# ---------------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------------
if [ "$#" -eq 0 ]; then
  refuse "no train name given (usage: scripts/train-push.sh <train-name> [--on <parent-train>] [--land | --restack | -- <smoke command>])"
fi

train_name="$1"
shift
case "$train_name" in
  -*) refuse "train name must not look like a flag (got '$train_name')" ;;
esac

land_only=0
restack=0
parent_arg=""
smoke_given=0
smoke_cmd=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --land) land_only=1; shift ;;
    --restack) restack=1; shift ;;
    --on)
      [ "$#" -ge 2 ] && [ -n "$2" ] || refuse "--on needs a parent train name"
      [ -z "$parent_arg" ] || refuse "--on given more than once"
      parent_arg="$2"
      shift 2
      ;;
    --)
      shift
      [ "$#" -gt 0 ] || refuse "-- given without a smoke command"
      smoke_given=1
      smoke_cmd=("$@")
      break
      ;;
    *) refuse "unexpected argument '$1' (use --on, --restack, --land, or put the smoke command after a bare --)" ;;
  esac
done
[ "$land_only" -eq 0 ] || {
  [ "$restack" -eq 0 ] || refuse "--land and --restack are mutually exclusive"
  [ "$smoke_given" -eq 0 ] || refuse "--land does not accept a smoke command"
}
if [ -n "$parent_arg" ]; then
  case "$parent_arg" in -*) refuse "parent train name must not look like a flag" ;; esac
  git -C "$REPO" check-ref-format "refs/heads/train/$parent_arg" || refuse "unusable parent train name '$parent_arg'"
  [ "$parent_arg" != "$train_name" ] || refuse "a train cannot be its own parent"
fi

train_ref="train/$train_name"
# Validate before the name reaches a refspec: a name with a space, a leading
# dot, or '..' in it produces a confusing git error deep in the push instead of
# a named refusal here.
if ! git -C "$REPO" check-ref-format "refs/heads/$train_ref"; then
  refuse "'$train_name' is not a usable branch name component"
fi

# REFUSE CONCURRENT USE OF ONE WORKTREE OR ONE TRAIN NAME. Stacked trains can
# overlap safely in separate worktrees: each owns its HEAD, its CI run, and its
# leased remote train ref. Two processes in one worktree still race rebases and
# resolve-HEAD; two using the same name race pushes and recovery. Atomic locks
# cover both resources, before any network work.
#
# A remote train/* ref is NOT the signal: a ref outlives its process (a red CI
# leaves the ref as the repush target, and the same train name is re-run round
# after round), and a ref arrives late (a train between resolve-HEAD and push
# holds no ref yet, so a second train in that window would be permitted - the
# exact concurrency to refuse). A pid is the direct observable. The lock below
# is a record, not a mutex: it means nothing unless its process is alive, so a
# SIGKILLed train (no trap runs) leaves a file the next scan clears. No EXIT
# trap on purpose - bash traps are global and a function further down sets one.
#
# Limit: the lock is local, refs are remote, so this sees trains on this box
# only. One operator, one box here; a second machine driving the same remote is
# invisible to it, which is why leftover refs are still listed below.
# (Lifted from BROCA's train-push, d588cb6c.)
# Absolute so locks and heartbeats remain addressable after the launch cwd is
# removed; later diagnostics can also name a path usable from anywhere.
#
# Two directories, because a linked worktree has both. `git_dir` is THIS
# worktree's (.git/worktrees/<name>): in-progress operations (MERGE_HEAD,
# rebase-merge/) live there and are per-worktree. `git_common_dir` is the
# repository's own .git, shared by every worktree: anything that is one fact
# per repository (the train-name lock, stack record, watch heartbeat, trigger proof,
# hooks) must live there, or each fresh worktree re-proves the trigger, two
# worktrees can push the same train at once, and the repo's pre-push hook is
# looked for in a directory that never has one.
git_dir="$(git -C "$REPO" rev-parse --absolute-git-dir 2>/dev/null)"
git_common_dir="$(cd "$REPO" && cd "$(git rev-parse --git-common-dir 2>/dev/null)" 2>/dev/null && pwd -P)"
[ -n "$git_common_dir" ] || git_common_dir="$git_dir"
train_key="$(printf '%s' "$train_name" | git -C "$REPO" hash-object --stdin)"
train_lock_dir="$git_dir/train-push-locks"
train_name_lock_dir="$git_common_dir/train-push-locks/by-name-$train_key"
watch_name="${train_name//\//-}"
watch_heartbeat="$git_common_dir/train-push-$watch_name.watch"
mkdir -p "$train_lock_dir" "$train_name_lock_dir" 2>/dev/null || true

# ACQUIRE BEFORE ANY NETWORK WORK. The lock directories hold owner files, but
# `mkdir` only prepares the directory: some shared filesystems let two clients
# both report success for the same new directory. The owner file itself is
# claimed with noclobber, whose exclusive create allows exactly one train in.
# Acquiring both locks before the ref scan prevents two trains from racing the
# resolve-HEAD-to-push window; a remote ref alone cannot represent a live train.
#
# The owner file is written after the directory exists, leaving a two-syscall
# window where the lock is held by someone who has not said who they are. An
# unreadable owner is treated as live and refused, never recovered: dispossessing
# a process mid-acquisition gives two live trains, the thing this guard exists
# to prevent; refusing costs one `rm -rf` the message names. Fail closed.
train_lock_attempts=0
# A pid alone cannot prove the owner is still the train: macOS recycles pids,
# and an unrelated process reusing the recorded pid made a finished train's
# leftover lock look live. The owner records its process start time too, and a
# pid whose start time differs is a different process.
train_lock_process_start() {
  ps -p "$1" -o lstart= 2>/dev/null | awk '{$1=$1; print}' || true
}
# `mkdir` is not reliably exclusive on every shared filesystem. The owner file
# uses noclobber's O_EXCL create so only one process can claim a lock directory.
train_lock_claim_file() {
  local train_lock_claim_path="$1"
  shift
  (
    set -o noclobber
    printf '%s\n' "$@" > "$train_lock_claim_path"
  ) 2>/dev/null
}
# Computed before taking the lock so the owner write right after mkdir stays a
# single write, keeping the window where the lock has no readable owner short.
train_lock_my_start="$(train_lock_process_start "$$")"
for train_lock_held in "$train_lock_dir/held" "$train_name_lock_dir/held"; do
train_lock_attempts=0
while true; do
  train_lock_held_created=0
  if mkdir "$train_lock_held" 2>/dev/null; then
    train_lock_held_created=1
  fi
  if [ -n "${TRAIN_PUSH_TEST_LOCK_CREATE_HOOK:-}" ] &&
    [ "$train_lock_held" = "$train_lock_dir/held" ]; then
    "$TRAIN_PUSH_TEST_LOCK_CREATE_HOOK" "$train_lock_held" "$train_name"
  fi
  if [ "$train_lock_held_created" = 1 ] &&
    train_lock_claim_file "$train_lock_held/owner" "$$ $train_name" "$train_lock_my_start"; then
    break
  fi

  owner_pid="$(awk 'NR==1{print $1}' "$train_lock_held/owner" 2>/dev/null || true)"
  owner_name="$(awk 'NR==1{print $2}' "$train_lock_held/owner" 2>/dev/null || true)"
  owner_start="$(awk 'NR==2' "$train_lock_held/owner" 2>/dev/null || true)"

  if [ -z "$owner_pid" ]; then
    printf 'train-push: the train lock is held with no readable owner.\n' >&2
    printf '  a train was killed between taking the lock and recording its pid.\n' >&2
    printf '  if you are sure no train is running:  rm -rf %s\n' "$train_lock_held" >&2
    refuse "train lock held by an unidentified owner; refusing rather than dispossessing it"
  fi

  owner_live=0
  if kill -0 "$owner_pid" 2>/dev/null; then
    if [ -n "$owner_start" ]; then
      [ "$(train_lock_process_start "$owner_pid")" = "$owner_start" ] && owner_live=1
    else
      # A lock written before start times were recorded: live only if the pid
      # is still running this script.
      ps -p "$owner_pid" -o command= 2>/dev/null | grep -q 'train-push' && owner_live=1
    fi
  fi

  if [ "$owner_live" = 1 ]; then
    printf 'train-push: train %s is RUNNING as pid %s\n' "${owner_name:-?}" "$owner_pid" >&2
    printf '  wait for it, or kill it if you know it is wedged.\n' >&2
    refuse "a train is already running on this machine; wait for it"
  fi

  # Stale owner. Recovery is serialized by a second exclusive owner-file claim,
  # `reap`. A rename alone was not enough: two trains reading the same stale owner
  # both renamed `held`, and the second rename took the first one's FRESH lock
  # (created by its mkdir after its own rename), so both trains proceeded.
  # While the reaper owner file exists, no other remover can claim `reap`; the
  # holder re-reads the `held` owner and removes it only if it still matches.
  # Both resources have their own recovery mutex. The name lock is shared
  # across worktrees, so its reaper must be shared across worktrees too.
  train_lock_reap="${train_lock_held%/held}/reap"
  if [ -n "${TRAIN_PUSH_TEST_LOCK_RACE_HOOK:-}" ]; then
    "$TRAIN_PUSH_TEST_LOCK_RACE_HOOK"
  fi
  train_lock_reap_claimed=0
  if mkdir "$train_lock_reap" 2>/dev/null &&
    train_lock_claim_file "$train_lock_reap/owner" "$$" "$train_lock_my_start"; then
    train_lock_reap_claimed=1
  fi
  if [ "$train_lock_reap_claimed" = 1 ]; then
    if [ "$(awk 'NR==1{print $1}' "$train_lock_held/owner" 2>/dev/null || true)" = "$owner_pid" ] &&
      [ "$(awk 'NR==2' "$train_lock_held/owner" 2>/dev/null || true)" = "$owner_start" ]; then
      rm -rf "$train_lock_held" 2>/dev/null || true
    fi
    if [ "$(awk 'NR==1' "$train_lock_reap/owner" 2>/dev/null || true)" = "$$" ] &&
      [ "$(awk 'NR==2' "$train_lock_reap/owner" 2>/dev/null || true)" = "$train_lock_my_start" ]; then
      rm -f "$train_lock_reap/owner" 2>/dev/null || true
      rmdir "$train_lock_reap" 2>/dev/null || true
    fi
  else
    reaper_pid="$(awk 'NR==1' "$train_lock_reap/owner" 2>/dev/null || true)"
    reaper_start="$(awk 'NR==2' "$train_lock_reap/owner" 2>/dev/null || true)"
    if [ -z "$reaper_pid" ] || ! kill -0 "$reaper_pid" 2>/dev/null ||
      [ "$(train_lock_process_start "$reaper_pid")" != "$reaper_start" ]; then
      # A reaper holds `reap` for a few milliseconds. One that is gone left
      # it behind; removing it here would reopen the race, so fail closed.
      printf 'train-push: a stale-lock recovery was interrupted.\n' >&2
      printf '  if you are sure no train is running:  rm -rf %s %s\n' "$train_lock_reap" "$train_lock_held" >&2
      refuse "train lock recovery left behind; refusing rather than racing it"
    fi
  fi

  train_lock_attempts=$((train_lock_attempts + 1))
  if [ "$train_lock_attempts" -ge 3 ]; then
    refuse "could not acquire the train lock after clearing a stale owner; try again"
  fi
done
done

# Remote refs outlive their processes. Some are pending parents, not leftovers;
# list them for the operator, never delete one just because no local pid owns it.
# A ref alone cannot race this
# train. They are listed with the delete composed, because the operator reaching
# this line is usually mid-CI-failure and composing `--delete train/<name>` by
# hand next to several refs is where the wrong one gets deleted.
stale_refs=$(git -C "$REPO" ls-remote --heads "$remote" 'train/*' 2>/dev/null | wc -l | tr -d ' ')
if [ "${stale_refs:-0}" -gt 0 ]; then
  printf 'train-push: %s train ref(s) on %s (pending or leftover):\n' \
    "$stale_refs" "$remote" >&2
  git -C "$REPO" ls-remote --heads "$remote" 'train/*' 2>/dev/null |
    sed "s|.*refs/heads/|    git push $remote --delete |" >&2
  printf '  (proceeding: a ref alone cannot race this train; do not delete a pending parent)\n' >&2
fi

# Operator tooling runs on the upstream gh; the shim is only for agent commands.
# Sourced up front because the trigger probe, the default-branch fallback, and
# watch-ci.sh all need it - failing here beats failing after a CI run.
# shellcheck source=lib/operator-gh.sh
source "$REPO/scripts/lib/operator-gh.sh" || exit 2

if [ -z "$repo_slug" ]; then
  repo_slug="$(repo_from_origin)" ||
    refuse "cannot derive the repository from $remote's URL; set REPO=owner/name"
fi
# watch-ci.sh receives the GitHub slug for its own REPO interface at each call;
# this process keeps REPO as the stable checkout path used by every git command.

# Read the default branch off the remote. Every later message, the moved-branch
# check and the fast-forward target all come from this one answer.
resolve_default_branch() {
  local ref name

  ref="$(git -C "$REPO" symbolic-ref -q "refs/remotes/$remote/HEAD" 2>/dev/null || true)"
  name="${ref#refs/remotes/"$remote"/}"
  if [ -n "$ref" ] && [ -n "$name" ] && [ "$name" != "$ref" ]; then
    printf '%s\n' "$name"
    return 0
  fi

  name="$(gh_query_or_refuse "default branch for $repo_slug" "$OPERATOR_GH" \
    repo view "$repo_slug" --json defaultBranchRef --jq '.defaultBranchRef.name')" || return "$?"
  [ -n "$name" ] || return 1
  printf '%s\n' "$name"
}

resolve_ci_run() {
  local sha="$1" branch="${2:-$train_ref}"
  local run_ids
  run_ids="$(gh_query_or_refuse "CI runs for $sha" "$OPERATOR_GH" run list \
    --repo "$repo_slug" --workflow "$tests_workflow_name" \
    --event "${WATCH_CI_EVENT:-push}" --limit 40 --json databaseId,headSha,headBranch \
    --jq ".[] | select(.headSha==\"$sha\" and .headBranch==\"$branch\") | .databaseId")" || return "$?"
  printf '%s\n' "$run_ids" | sed -n '1p'
}

ci_run_attempt() {
  gh_query_or_refuse "attempt for CI run $1" "$OPERATOR_GH" run view "$1" \
    --repo "$repo_slug" --json attempt --jq '.attempt'
}

wait_for_new_run_attempt() {
  local run_id="$1"
  local previous_attempt="$2"
  local attempts="${TRAIN_PUSH_RERUN_ATTEMPTS:-30}"
  local wait_seconds="${TRAIN_PUSH_RERUN_SLEEP:-2}"
  local current_attempt
  for _ in $(seq 1 "$attempts"); do
    current_attempt="$(ci_run_attempt "$run_id")" || return "$?"
    if [[ "$current_attempt" =~ ^[0-9]+$ ]] && [ "$current_attempt" -gt "$previous_attempt" ]; then
      printf '%s\n' "$current_attempt"
      return 0
    fi
    sleep "$wait_seconds"
  done
  printf 'train-push: rerun request for %s did not advance its attempt beyond %s\n' \
    "$run_id" "$previous_attempt" >&2
  return 1
}

wait_for_run_attempt_completion() {
  local run_id="$1"
  local attempt="$2"
  local attempts="${TRAIN_PUSH_RERUN_WAIT_ATTEMPTS:-360}"
  local wait_seconds="${TRAIN_PUSH_RERUN_WAIT_SLEEP:-10}"
  local status
  for _ in $(seq 1 "$attempts"); do
    status="$(gh_query_or_refuse "status for CI run $run_id attempt $attempt" "$OPERATOR_GH" \
      run view "$run_id" --repo "$repo_slug" --attempt "$attempt" \
      --json status --jq '.status')" || return "$?"
    [ "$status" = "completed" ] && return 0
    sleep "$wait_seconds"
  done
  printf 'train-push: rerun attempt %s for run %s did not complete before the wait limit\n' \
    "$attempt" "$run_id" >&2
  return 1
}

rerun_existing_run() {
  local run_id="$1"
  local old_attempt="$2"
  local bad_jobs line conclusion job_id
  local current_attempt="$old_attempt"
  local remaining_actions
  local -a failed_job_ids=() cancelled_job_ids=()

  bad_jobs="$(gh_query_or_refuse "jobs for CI run $run_id" "$OPERATOR_GH" \
    run view "$run_id" --repo "$repo_slug" --json jobs \
    --jq '[.jobs[] | select(.conclusion=="failure" or .conclusion=="cancelled") | .conclusion + "|" + (.databaseId|tostring)] | .[]')" || return "$?"
  while IFS='|' read -r conclusion job_id; do
    [ -n "$job_id" ] || continue
    case "$job_id" in *[!0-9]*) continue ;; esac
    case "$conclusion" in
      failure) failed_job_ids+=("$job_id") ;;
      cancelled) cancelled_job_ids+=("$job_id") ;;
    esac
  done <<< "$bad_jobs"

  if [ "${#failed_job_ids[@]}" -eq 0 ] && [ "${#cancelled_job_ids[@]}" -eq 0 ]; then
    printf 'train-push: completed run %s has no failed or cancelled jobs to rerun; refusing to treat its old conclusion as this push result\n' \
      "$run_id" >&2
    return 1
  fi
  remaining_actions=$(( ${#failed_job_ids[@]} > 0 ? 1 : 0 ))
  remaining_actions=$((remaining_actions + ${#cancelled_job_ids[@]}))

  if [ "${#failed_job_ids[@]}" -gt 0 ]; then
    GH_SHIM_BYPASS=operator "$OPERATOR_GH" run rerun "$run_id" --failed >&2 || {
      printf 'train-push: could not rerun failed jobs for run %s\n' "$run_id" >&2
      return 1
    }
    current_attempt="$(wait_for_new_run_attempt "$run_id" "$current_attempt")" || return "$?"
    remaining_actions=$((remaining_actions - 1))
    if [ "$remaining_actions" -gt 0 ]; then
      wait_for_run_attempt_completion "$run_id" "$current_attempt" || return "$?"
    fi
  fi
  if [ "${#cancelled_job_ids[@]}" -gt 0 ]; then
    for job_id in "${cancelled_job_ids[@]}"; do
      # `--failed` does not include cancelled jobs, so each is rerun explicitly.
      GH_SHIM_BYPASS=operator "$OPERATOR_GH" run rerun "$run_id" --job "$job_id" >&2 || {
        printf 'train-push: could not rerun cancelled job %s for run %s\n' "$job_id" "$run_id" >&2
        return 1
      }
      current_attempt="$(wait_for_new_run_attempt "$run_id" "$current_attempt")" || return "$?"
      remaining_actions=$((remaining_actions - 1))
      if [ "$remaining_actions" -gt 0 ]; then
        wait_for_run_attempt_completion "$run_id" "$current_attempt" || return "$?"
      fi
    done
  fi
  printf '%s\n' "$current_attempt"
}

ci_run_url() {
  gh_query_or_refuse "URL for CI run $1" "$OPERATOR_GH" run view "$1" \
    --repo "$repo_slug" --json url --jq '.url'
}

report_existing_red() {
  local sha="$1"
  local run_url="$2"
  printf 'train-push: CI red for previously pushed %s\n' "$sha" >&2
  [ -z "$run_url" ] || printf 'train-push: run %s\n' "$run_url" >&2
  printf 'train-push: %s/%s still holds %s — fix, commit, and re-run: scripts/train-push.sh %s\n' \
    "$remote" "$train_ref" "$sha" "$train_name" >&2
}

landing_failed() {
  local reason="$1"
  local sha="$2"
  local run_url="$3"
  {
    printf 'train-push: CI green, but landing failed — %s\n' "$reason"
    printf '  green run: %s\n' "$run_url"
    printf '  train sha: %s\n' "$sha"
    printf '  finish without re-running CI:\n'
    printf '    scripts/train-push.sh %s --land\n' "$train_name"
  } >&2
  exit 4
}

require_repo_after_green() {
  local sha="$1"
  local run_url="$2"
  if [ ! -d "$REPO" ]; then
    landing_failed "repository path $REPO no longer exists" "$sha" "$run_url"
  fi
}

process_start_time() {
  ps -p "$1" -o lstart= 2>/dev/null | awk '{$1=$1; print}' || true
}

read_watcher_heartbeat() {
  heartbeat_pid=""
  heartbeat_start=""
  heartbeat_updated=""
  heartbeat_dead_note=""
  [ -f "$watch_heartbeat" ] || return 0

  heartbeat_pid="$(sed -n 's/^pid=//p' "$watch_heartbeat" | head -1)"
  heartbeat_start="$(sed -n 's/^start=//p' "$watch_heartbeat" | head -1)"
  heartbeat_updated="$(sed -n 's/^updated=//p' "$watch_heartbeat" | head -1)"
  case "$heartbeat_pid" in
    '' | *[!0-9]*) heartbeat_now="" ;;
    *) heartbeat_now="$(process_start_time "$heartbeat_pid")" ;;
  esac

  if [ -n "$heartbeat_now" ] && [ "$heartbeat_now" = "$heartbeat_start" ] && kill -0 "$heartbeat_pid" 2>/dev/null; then
    refuse "watcher $heartbeat_pid is still running for $remote/$train_ref; wait for it"
  fi

  heartbeat_dead_note="watcher ${heartbeat_pid:-unknown} died at ${heartbeat_updated:-an unknown time}"
}

refuse_non_fast_forward() {
  local sha="$1"
  refuse "$sha is not a fast-forward of $remote/$default_branch; rebase and re-run: scripts/train-push.sh $train_name"
}

# Stack identity survives a parent ref deletion or rewrite and is shared by
# linked worktrees. It is data, never sourced as shell code. Hashing the train
# name avoids both path traversal and collisions such as a/b versus a-b.
stack_dir="$git_common_dir/train-push-stacks"
stack_record="$stack_dir/$train_key"
parent_name=""
parent_base=""

save_stack() {
  if [ -z "$parent_name" ]; then
    rm -f "$stack_record"
    return
  fi
  mkdir -p "$stack_dir"
  printf '%s\n%s\n' "$parent_name" "$parent_base" > "$stack_record.tmp.$$"
  mv "$stack_record.tmp.$$" "$stack_record"
}

resolve_stack() {
  local sha="$1" saved_name="" saved_base="" tip ref distance nearest=""
  local best_distance="" ambiguous=0
  if [ -f "$stack_record" ]; then
    { IFS= read -r saved_name; IFS= read -r saved_base; } < "$stack_record" ||
      refuse "unreadable stack record $stack_record"
    git -C "$REPO" check-ref-format "refs/heads/train/$saved_name" >/dev/null ||
      refuse "invalid parent in stack record $stack_record"
    git -C "$REPO" merge-base --is-ancestor "$saved_base" "$sha" ||
      refuse "recorded base $saved_base is not an ancestor of this train; reconstruct the stack by hand"
    parent_name="$saved_name"
    parent_base="$saved_base"
    if [ -n "$parent_arg" ] && [ "$parent_arg" != "$saved_name" ]; then
      [ "$restack" -eq 1 ] || refuse "recorded parent is $saved_name; changing it to $parent_arg needs --restack"
    fi
    return
  fi
  [ "$restack" -eq 0 ] || refuse "--restack needs a recorded stack base; reconstruct the stack by hand"
  if [ -n "$parent_arg" ]; then
    parent_name="$parent_arg"
    parent_base="$(git -C "$REPO" rev-parse --verify -q "refs/remotes/$remote/train/$parent_name" || true)"
    [ -n "$parent_base" ] || refuse "no $remote/train/$parent_name; push the parent first"
    [ "$parent_base" != "$sha" ] && git -C "$REPO" merge-base --is-ancestor "$parent_base" "$sha" ||
      refuse "parent $parent_name tip $parent_base is not strictly below the child; restack by hand first"
    return
  fi
  # Choose the closest proper ancestor, not merely the first branch printed.
  # That makes N+2 depend on N+1 instead of accidentally depending only on N.
  while read -r ref tip; do
    [ "$ref" != "refs/remotes/$remote/$train_ref" ] || continue
    [ "$tip" != "$sha" ] || continue
    git -C "$REPO" merge-base --is-ancestor "$tip" "$remote_default" && continue
    git -C "$REPO" merge-base --is-ancestor "$tip" "$sha" || continue
    distance="$(git -C "$REPO" rev-list --count "$tip..$sha")"
    if [ -z "$best_distance" ] || [ "$distance" -lt "$best_distance" ]; then
      nearest="$ref"; parent_base="$tip"; best_distance="$distance"; ambiguous=0
    elif [ "$distance" -eq "$best_distance" ]; then
      ambiguous=1
    fi
  done < <(git -C "$REPO" for-each-ref --format='%(refname) %(objectname)' "refs/remotes/$remote/train/")
  [ "$ambiguous" -eq 0 ] || refuse "ambiguous parent train tips; name the parent with --on"
  if [ -n "$nearest" ]; then
    parent_name="${nearest#refs/remotes/"$remote"/train/}"
    say "detected parent $parent_name at $parent_base"
  fi
}

refuse_stack() {
  refuse "$1; base $parent_base is not landable on $remote/$default_branch; restack $train_name with: scripts/train-push.sh $train_name --restack"
}

wait_for_parent() {
  local sha="$1" run_url="$2" main_sha tip rid verdict status conclusion bad refs ref ref_sha
  local attempts="${TRAIN_PUSH_PARENT_ATTEMPTS:-360}" wait_seconds="${TRAIN_PUSH_PARENT_SLEEP:-10}" parent_poll
  [[ "$attempts" =~ ^[1-9][0-9]*$ ]] || refuse "TRAIN_PUSH_PARENT_ATTEMPTS must be a positive integer"
  for ((parent_poll=1; parent_poll<=attempts; parent_poll++)); do
    # Read advertised heads without updating shared remote-tracking refs. The
    # parent and descendants can poll together without contending on ref locks.
    refs="$(git -C "$REPO" ls-remote --heads "$remote" "refs/heads/$default_branch" "refs/heads/train/$parent_name")" ||
      landing_failed "could not read remote heads while waiting for parent $parent_name" "$sha" "$run_url"
    main_sha=""; tip=""
    while read -r ref_sha ref; do
      case "$ref" in
        "refs/heads/$default_branch") main_sha="$ref_sha" ;;
        "refs/heads/train/$parent_name") tip="$ref_sha" ;;
      esac
    done <<< "$refs"
    [ -n "$main_sha" ] ||
      landing_failed "remote default branch $default_branch disappeared while waiting for parent $parent_name" "$sha" "$run_url"
    # A peer may have landed a commit outside this child's history. Download
    # its advertised object only when needed for ancestry checks, without
    # changing any tracking ref or a train's FETCH_HEAD.
    if ! git -C "$REPO" cat-file -e "$main_sha^{commit}" 2>/dev/null; then
      fetch_remote --no-write-fetch-head --no-tags --refmap= "$remote" "$main_sha" ||
        landing_failed "could not fetch remote default sha while waiting for parent $parent_name" "$sha" "$run_url"
    fi
    # A child run covers parent+child, but cannot authorize landing the parent
    # itself. Its exact base must already have arrived on the default branch.
    if git -C "$REPO" merge-base --is-ancestor "$parent_base" "$main_sha"; then
      git -C "$REPO" merge-base --is-ancestor "$main_sha" "$sha" ||
        refuse_stack "parent $parent_name landed at a different sha, or $remote/$default_branch moved beyond the tested stack"
      say "parent $parent_name base $parent_base is on $remote/$default_branch; spending the child's own green without rerunning CI"
      return
    fi
    [ -n "$tip" ] || refuse_stack "parent $parent_name is abandoned or landed at a different sha"
    [ "$tip" = "$parent_base" ] || refuse_stack "parent $parent_name changed to $tip"
    rid="$(resolve_ci_run "$parent_base" "train/$parent_name")" || return "$?"
    if [ -n "$rid" ]; then
      verdict="$(gh_query_or_refuse "verdict for parent CI run $rid" "$OPERATOR_GH" \
        run view "$rid" --repo "$repo_slug" --json status,conclusion \
        --jq '[.status, (.conclusion // "")] | @tsv')" || return "$?"
      IFS=$'\t' read -r status conclusion <<< "$verdict"
      bad="$(gh_query_or_refuse "jobs for parent CI run $rid" "$OPERATOR_GH" \
        run view "$rid" --repo "$repo_slug" --json jobs \
        --jq '[.jobs[] | select(.conclusion=="failure") | .name] | join("; ")')" || return "$?"
      if { [ -n "$bad" ] && [ "$bad" != null ]; } ||
        { [ "$status" = completed ] && [ "$conclusion" != success ]; }; then
        refuse "parent $parent_name is red; fix and repush it, then restack $train_name (own CI green: $run_url; scripts/train-push.sh $train_name --restack)"
      fi
    fi
    if [ "$parent_poll" -eq 1 ]; then
      say "waiting for parent $parent_name to land exactly at $parent_base (currently $remote/$default_branch=$main_sha; bounded to $attempts polls, ${wait_seconds}s apart; own CI green: $run_url)"
    fi
    [ "$parent_poll" -eq "$attempts" ] || sleep "$wait_seconds"
  done
  refuse "parent $parent_name has not landed base $parent_base before the wait limit; own CI green: $run_url; resume: scripts/train-push.sh $train_name --land"
}

# watch-ci also reports fail-fast job failures. A watch returning zero is not
# sufficient by itself: only a completed success for this exact sha AND branch
# authorizes landing, including after a rerun attempt.
require_ci_success() {
  local sha="$1" rid verdict
  rid="$(resolve_ci_run "$sha")"
  [ -n "$rid" ] || refuse "no CI run resolved for tested $remote/$train_ref at $sha"
  verdict="$(gh_query_or_refuse "verdict for CI run $rid" "$OPERATOR_GH" \
    run view "$rid" --repo "$repo_slug" --json status,conclusion \
    --jq '[.status, (.conclusion // "")] | @tsv')" || return "$?"
  [ "$verdict" = $'completed\tsuccess' ] || refuse "CI run $rid for $sha did not conclude success; not landing"
}

# Branch protection can require checks from more than the workflow run watched
# above. Read every required context for this exact sha before either landing
# path pushes it; a missing result is pending, not evidence of a green check.
REQUIRED_CONTEXTS=()
REQUIRED_CONTEXTS_ERROR=""
CHECK_RUN_RESULTS=""
COMMIT_STATUS_RESULTS=""
REQUIRED_MISSING=()
REQUIRED_PENDING=()
REQUIRED_FAILED=()
REQUIRED_SUCCESS=()

read_required_contexts() {
  local output context existing found
  REQUIRED_CONTEXTS=()
  REQUIRED_CONTEXTS_ERROR=""
  if output="$(gh_query_or_refuse "branch protection required checks for $remote/$default_branch" \
      "$OPERATOR_GH" api "repos/$repo_slug/branches/$default_branch/protection/required_status_checks" \
      --jq 'if ((.checks // []) | length) > 0 then .checks[].context else .contexts[]? end' 2>&1)"; then
    while IFS= read -r context; do
      [ -n "$context" ] || continue
      found=0
      if [ "${#REQUIRED_CONTEXTS[@]}" -gt 0 ]; then
        for existing in "${REQUIRED_CONTEXTS[@]}"; do
          [ "$existing" = "$context" ] && found=1 && break
        done
      fi
      [ "$found" -eq 1 ] || REQUIRED_CONTEXTS+=("$context")
    done <<< "$output"
    return 0
  fi
  REQUIRED_CONTEXTS_ERROR="$(printf '%s' "$output" | tr '\n' ' ')"
  return 1
}

read_required_check_results() {
  local sha="$1"
  CHECK_RUN_RESULTS="$(gh_query_or_refuse "check runs for required contexts on $sha" "$OPERATOR_GH" \
    api --paginate "repos/$repo_slug/commits/$sha/check-runs?per_page=100" \
    --jq '.check_runs[] | [(.name // ""), (.status // ""), (.conclusion // "-"), (.created_at // "")] | @tsv')" || return "$?"
  COMMIT_STATUS_RESULTS="$(gh_query_or_refuse "commit statuses for required contexts on $sha" "$OPERATOR_GH" \
    api --paginate "repos/$repo_slug/commits/$sha/status?per_page=100" \
    --jq '.statuses[] | [(.context // ""), (.state // ""), (.created_at // "")] | @tsv')" || return "$?"
}

# The same context can be reported by multiple runs on one sha. Use the newest
# result, and prefer pending or failure over success when timestamps tie.
LATEST_REQUIRED_TIME=""
LATEST_REQUIRED_STATE=""
record_latest_required_state() {
  local candidate_time="$1" candidate_state="$2"
  if [ -z "$LATEST_REQUIRED_STATE" ] || [[ "$candidate_time" > "$LATEST_REQUIRED_TIME" ]]; then
    LATEST_REQUIRED_TIME="$candidate_time"
    LATEST_REQUIRED_STATE="$candidate_state"
  elif [ "$candidate_time" = "$LATEST_REQUIRED_TIME" ]; then
    case "$LATEST_REQUIRED_STATE:$candidate_state" in
      success:pending|success:failed|pending:failed)
        LATEST_REQUIRED_STATE="$candidate_state"
        ;;
    esac
  fi
}

classify_required_check_results() {
  local context result_name result_status result_conclusion result_created_at result_state
  REQUIRED_MISSING=()
  REQUIRED_PENDING=()
  REQUIRED_FAILED=()
  REQUIRED_SUCCESS=()
  for context in "${REQUIRED_CONTEXTS[@]}"; do
    LATEST_REQUIRED_TIME=""
    LATEST_REQUIRED_STATE=""
    while IFS=$'\t' read -r result_name result_status result_conclusion result_created_at; do
      [ "$result_name" = "$context" ] || continue
      if [ "$result_status" = "completed" ]; then
        if [ "$result_conclusion" = "success" ]; then
          result_state="success"
        else
          result_state="failed"
        fi
      else
        result_state="pending"
      fi
      record_latest_required_state "$result_created_at" "$result_state"
    done <<< "$CHECK_RUN_RESULTS"
    while IFS=$'\t' read -r result_name result_status result_created_at; do
      [ "$result_name" = "$context" ] || continue
      case "$result_status" in
        success) result_state="success" ;;
        failure|error) result_state="failed" ;;
        *) result_state="pending" ;;
      esac
      record_latest_required_state "$result_created_at" "$result_state"
    done <<< "$COMMIT_STATUS_RESULTS"
    case "$LATEST_REQUIRED_STATE" in
      failed) REQUIRED_FAILED+=("$context") ;;
      pending) REQUIRED_PENDING+=("$context") ;;
      success) REQUIRED_SUCCESS+=("$context") ;;
      *) REQUIRED_MISSING+=("$context") ;;
    esac
  done
}

join_contexts() {
  local separator="" context
  for context in "$@"; do
    printf '%s%s' "$separator" "$context"
    separator=", "
  done
}

required_check_state_details() {
  local details=""
  if [ "${#REQUIRED_FAILED[@]}" -gt 0 ]; then
    details="failed: $(join_contexts "${REQUIRED_FAILED[@]}")"
  fi
  if [ "${#REQUIRED_PENDING[@]}" -gt 0 ]; then
    [ -z "$details" ] || details+=", "
    details+="pending: $(join_contexts "${REQUIRED_PENDING[@]}")"
  fi
  if [ "${#REQUIRED_MISSING[@]}" -gt 0 ]; then
    [ -z "$details" ] || details+=", "
    details+="missing: $(join_contexts "${REQUIRED_MISSING[@]}")"
  fi
  if [ -z "$details" ] && [ "${#REQUIRED_SUCCESS[@]}" -gt 0 ]; then
    details="successful: $(join_contexts "${REQUIRED_SUCCESS[@]}")"
  fi
  printf '%s' "${details:-no required check states were available}"
}

wait_for_required_status_checks() {
  local sha="$1"
  local run_url="$2"
  local attempts="${TRAIN_PUSH_REQUIRED_CHECK_ATTEMPTS:-360}"
  local wait_seconds="${TRAIN_PUSH_REQUIRED_CHECK_SLEEP:-10}"
  local result_rc attempt details contexts

  if ! read_required_contexts; then
    say "could not read branch protection required checks for $remote/$default_branch; continuing with existing CI-only landing behavior (${REQUIRED_CONTEXTS_ERROR:-query failed})"
    return 0
  fi
  if [ "${#REQUIRED_CONTEXTS[@]}" -eq 0 ]; then
    say "no required status checks reported for $remote/$default_branch; continuing with existing CI-only landing behavior"
    return 0
  fi
  [[ "$attempts" =~ ^[0-9]+$ ]] && [ "$attempts" -gt 0 ] ||
    refuse "TRAIN_PUSH_REQUIRED_CHECK_ATTEMPTS must be a positive integer"
  [[ "$wait_seconds" =~ ^[0-9]+$ ]] ||
    refuse "TRAIN_PUSH_REQUIRED_CHECK_SLEEP must be a nonnegative integer"

  contexts="$(join_contexts "${REQUIRED_CONTEXTS[@]}")"
  for attempt in $(seq 1 "$attempts"); do
    if read_required_check_results "$sha"; then
      :
    else
      result_rc=$?
      landing_failed "could not read required check results for $sha (gh query exit $result_rc)" "$sha" "$run_url"
    fi
    classify_required_check_results
    if [ "${#REQUIRED_FAILED[@]}" -gt 0 ]; then
      landing_failed "required checks not all green on $sha ($(required_check_state_details))" "$sha" "$run_url"
    fi
    if [ "${#REQUIRED_PENDING[@]}" -eq 0 ] && [ "${#REQUIRED_MISSING[@]}" -eq 0 ]; then
      say "required status checks green on $sha: $contexts"
      return 0
    fi
    if [ "$attempt" -eq 1 ]; then
      say "waiting for required status checks on $sha ($(required_check_state_details)); bounded wait is $attempts polls"
    fi
    [ "$attempt" -lt "$attempts" ] && sleep "$wait_seconds"
  done
  details="$(required_check_state_details)"
  landing_failed "required checks did not all complete successfully on $sha before the wait limit ($details)" "$sha" "$run_url"
}

required_check_failure_reason() {
  local sha="$1" result_rc details
  if ! read_required_contexts; then
    printf 'refused: required checks not all green on %s (branch protection requirements could not be read: %s)' \
      "$sha" "${REQUIRED_CONTEXTS_ERROR:-query failed}"
    return
  fi
  if [ "${#REQUIRED_CONTEXTS[@]}" -eq 0 ]; then
    printf 'refused: required checks not all green on %s (branch protection reported no required contexts)' "$sha"
    return
  fi
  if read_required_check_results "$sha"; then
    classify_required_check_results
    details="$(required_check_state_details)"
    printf 'refused: required checks not all green on %s (%s)' "$sha" "$details"
  else
    result_rc=$?
    printf 'refused: required checks not all green on %s (could not read required check states; gh query exit %s)' \
      "$sha" "$result_rc"
  fi
}

is_governed_align_commit() {
  local commit="$1" subject path count=0
  subject="$(git -C "$REPO" show -s --format=%s "$commit")" || return 1
  case "$subject" in
    "Align governed publication manifests"|"Align release manifest and evidence") ;;
    *) return 1 ;;
  esac
  while IFS= read -r path; do
    [ -n "$path" ] || continue
    case "$path" in
      docs/v0.49-agent-prefix-capture.json|\
      docs/v0.49-agent-surface-manifest.json|\
      docs/v0.49-legacy-vocabulary-allowlist.json|\
      docs/v0.49-release-evidence.json|\
      docs/v0.49-release-manifest.json) ;;
      *) return 1 ;;
    esac
    count=$((count + 1))
  done < <(git -C "$REPO" diff-tree --root --no-commit-id --no-renames --name-only -r "$commit")
  [ "$count" -gt 0 ]
}

restack_child() {
  local target target_name old_base="$parent_base" merges before="$head_sha"
  local commit align_count=0
  local align_tmp align_drop_file align_editor
  local -a align_drop_shas=()
  target_name="${parent_arg:-$parent_name}"
  target="$(git -C "$REPO" rev-parse --verify -q "refs/remotes/$remote/train/$target_name" || true)"
  if [ -z "$target" ]; then
    target="$(git -C "$REPO" rev-parse "$remote_default")"
    target_name=""
  fi
  git -C "$REPO" merge-base --is-ancestor "$remote_default" "$target" ||
    refuse "restack target is not based on $remote/$default_branch; restack its parent first"
  [ "$target" != "$before" ] && ! git -C "$REPO" merge-base --is-ancestor "$before" "$target" ||
    refuse "cannot restack onto a train that already contains this child"
  merges="$(git -C "$REPO" rev-list --merges "$old_base..$head_sha")"
  [ -z "$merges" ] || refuse "child carries merge commit(s); not rebasing because merge-only resolutions would be lost; restack by hand"
  for commit in $(git -C "$REPO" rev-list --reverse "$old_base..$before"); do
    if is_governed_align_commit "$commit"; then
      align_drop_shas+=("$commit")
      align_count=$((align_count + 1))
    fi
  done
  say "restacking $train_name: replay only $old_base..$head_sha onto $target"
  if [ "$align_count" -gt 0 ]; then
    align_tmp="$(mktemp -d "${TMPDIR:-/tmp}/train-push-align.XXXXXX")"
    align_drop_file="$align_tmp/drop-commits"
    align_editor="$align_tmp/sequence-editor"
    printf '%s\n' "${align_drop_shas[@]}" > "$align_drop_file"
    cat > "$align_editor" <<'EDITOR'
#!/usr/bin/env bash
set -euo pipefail
python3 - "$1" "${TRAIN_PUSH_ALIGN_DROP_FILE:?}" <<'PY'
import pathlib, re, sys

todo = pathlib.Path(sys.argv[1])
drops = [line.strip() for line in pathlib.Path(sys.argv[2]).read_text().splitlines() if line.strip()]
rewritten = []
for line in todo.read_text().splitlines(keepends=True):
    ending = line[len(line.rstrip("\r\n")):]
    content = line[:-len(ending)] if ending else line
    match = re.match(r"^(\s*)pick(\s+)([0-9a-f]+)(.*)$", content)
    if match and any(commit.startswith(match.group(3)) for commit in drops):
        content = f"{match.group(1)}drop{match.group(2)}{match.group(3)}{match.group(4)}"
    rewritten.append(content + ending)
todo.write_text("".join(rewritten))
PY
EDITOR
    chmod +x "$align_editor"
    if ! TRAIN_PUSH_ALIGN_DROP_FILE="$align_drop_file" GIT_SEQUENCE_EDITOR="$align_editor" \
      git -C "$REPO" rebase -i --onto "$target" "$old_base"; then
      rm -rf "$align_tmp"
      git -C "$REPO" rebase --abort >/dev/null 2>&1 || true
      refuse "restack conflicted; rebase aborted (was $before); resolve by hand and repush"
    fi
    rm -rf "$align_tmp"
  elif ! git -C "$REPO" rebase --onto "$target" "$old_base"; then
    git -C "$REPO" rebase --abort >/dev/null 2>&1 || true
    refuse "restack conflicted; rebase aborted (was $before); resolve by hand and repush"
  fi
  if [ "$align_count" -gt 0 ]; then
    say "checking publication inputs after dropping $align_count obsolete generated Align commit(s)"
    if ! (cd "$REPO" && node scripts/release-gate-v049.mjs --check-inputs); then
      refuse "publication input check failed after restack; inspect the rebased tree before repushing"
    fi
  fi
  head_sha="$(git -C "$REPO" rev-parse HEAD)"
  [ "$head_sha" != "$before" ] || refuse "restack did not change the sha; resume with scripts/train-push.sh $train_name (same-sha CI recovery)"
  parent_name="$target_name"
  parent_base="$target"
  save_stack
}

land_verified_sha() {
  local sha="$1"
  local run_url="$2"

  require_repo_after_green "$sha" "$run_url"
  fetch_remote -q "$remote" "$default_branch" ||
    landing_failed "git fetch $remote $default_branch failed" "$sha" "$run_url"
  if [ -n "$parent_name" ]; then
    wait_for_parent "$sha" "$run_url"
  fi
  if ! git -C "$REPO" merge-base --is-ancestor "$remote_default" "$sha"; then
    refuse_non_fast_forward "$sha"
  fi

  wait_for_required_status_checks "$sha" "$run_url"
  say "landing CI-verified $sha on $remote/$default_branch"
  set +e
  git -C "$REPO" push "$remote" "$sha:refs/heads/$default_branch" 2>&1 | tee "$push_log"
  land_rc="${PIPESTATUS[0]}"
  set -e
  if [ "$land_rc" -ne 0 ]; then
    land_reason="push of $sha to $remote/$default_branch failed; $remote/$train_ref still holds the tested sha"
    if grep -qE 'GH006|equired status check|rotected branch update failed' "$push_log"; then
      land_reason="$(required_check_failure_reason "$sha"); merge onto the train branch and push there; CI runs on the merge sha, then main fast-forwards."
    fi
    landing_failed "$land_reason" "$sha" "$run_url"
  fi

  fetch_remote -q "$remote" "$default_branch" ||
    landing_failed "could not verify $remote/$default_branch after push" "$sha" "$run_url"
  if ! git -C "$REPO" merge-base --is-ancestor "$sha" "$remote_default"; then
    landing_failed "push reported success but $sha is not on $remote/$default_branch" "$sha" "$run_url"
  fi

  say "landed previously-verified sha $sha from $run_url on $remote/$default_branch"
  if ! git -C "$REPO" push -q "$remote" --delete "$train_ref"; then
    printf 'train-push: warning — could not delete %s/%s (delete it by hand)\n' "$remote" "$train_ref" >&2
  fi
  rm -f "$watch_heartbeat"
  say "done"
}

watch_log="$(mktemp "${TMPDIR:-/tmp}/train-push-watch.XXXXXX")"
push_log="$(mktemp "${TMPDIR:-/tmp}/train-push-push.XXXXXX")"
trap 'rm -f "$watch_log" "$push_log" "$TRAIN_PUSH_EXEC_COPY"' EXIT

fetch_remote -q --prune "$remote" || refuse "git fetch $remote failed"
if default_branch="$(resolve_default_branch)"; then
  :
else
  default_branch_rc=$?
  [ "$default_branch_rc" -ne 1 ] ||
    refuse "could not determine $remote's default branch (run: git remote set-head $remote -a)"
  exit "$default_branch_rc"
fi
if [ -z "$default_branch" ]; then
  refuse "could not determine $remote's default branch (run: git remote set-head $remote -a)"
fi
remote_default="refs/remotes/$remote/$default_branch"
if ! git -C "$REPO" rev-parse --verify -q "$remote_default" >/dev/null; then
  refuse "no $remote/$default_branch to land on"
fi

head_sha="$(git -C "$REPO" rev-parse HEAD)"
train_remote_sha=""
if git -C "$REPO" rev-parse --verify -q "refs/remotes/$remote/$train_ref" >/dev/null; then
  train_remote_sha="$(git -C "$REPO" rev-parse "refs/remotes/$remote/$train_ref")"
fi

stack_sha="$head_sha"
[ "$land_only" -eq 0 ] || stack_sha="${train_remote_sha:-$head_sha}"
resolve_stack "$stack_sha"
if [ -n "$parent_name" ]; then
  say "stacked on parent $parent_name at base $parent_base"
fi

recover_existing=0
if [ "$land_only" -eq 1 ]; then
  [ -n "$train_remote_sha" ] || refuse "--land needs an existing $remote/$train_ref"
  recover_existing=1
elif [ "$restack" -eq 0 ] && [ -n "$train_remote_sha" ] && [ "$train_remote_sha" = "$head_sha" ]; then
  recover_existing=1
fi

if [ "$recover_existing" -eq 1 ]; then
  verified_sha="$train_remote_sha"
  read_watcher_heartbeat
  run_id="$(resolve_ci_run "$verified_sha")"
  run_url=""
  run_status=""
  run_conclusion=""
  rerun_attempt=""

  if [ -n "$run_id" ]; then
    verdict="$(gh_query_or_refuse "verdict for CI run $run_id" "$OPERATOR_GH" \
      run view "$run_id" --repo "$repo_slug" \
      --json status,conclusion --jq '[.status, (.conclusion // "")] | @tsv')" || exit "$?"
    IFS=$'\t' read -r run_status run_conclusion <<< "$verdict"
    run_url="$(ci_run_url "$run_id")"
  fi

  if [ "$run_status" = "completed" ] && [ "$run_conclusion" != "success" ]; then
    old_attempt="$(ci_run_attempt "$run_id")" || exit "$?"
    if ! [[ "$old_attempt" =~ ^[0-9]+$ ]]; then
      refuse "could not read the current attempt for completed run $run_id"
    fi
    rerun_attempt="$(rerun_existing_run "$run_id" "$old_attempt")" || exit "$?"
    say "sha already ran in ${run_url:-run $run_id} ($run_conclusion); rerunning its failed and cancelled jobs (attempt $rerun_attempt)"
  fi

  if [ "$run_status" != "completed" ] || [ -n "$rerun_attempt" ]; then
    watch_target="${run_id:-$verified_sha}"
    if [ -z "$rerun_attempt" ]; then
      say "attaching to CI for existing $remote/$train_ref at $verified_sha"
    fi
    set +e
    (cd "$REPO" && REPO="$repo_slug" WATCH_CI_BRANCH="$train_ref" WATCH_CI_HEARTBEAT="$watch_heartbeat" \
      TRAIN_PUSH_QUEUED_LIMIT_MIN="${TRAIN_PUSH_QUEUED_LIMIT_MIN:-10}" \
      WATCH_CI_ATTEMPT="$rerun_attempt" \
      "$script_dir/watch-ci.sh" "$watch_target") 2>&1 | tee "$watch_log"
    watch_rc="${PIPESTATUS[0]}"
    set -e
    run_url="$(grep -m1 '^CI_RUN_URL ' "$watch_log" 2>/dev/null | sed 's/^CI_RUN_URL //' || true)"
    if [ "$watch_rc" -ne 0 ]; then
      if [ "$watch_rc" -eq 1 ]; then
        report_existing_red "$verified_sha" "$run_url"
        exit 1
      fi
      refuse "could not watch CI for existing $remote/$train_ref at $verified_sha (watch-ci exit $watch_rc)"
    fi
  fi

  require_ci_success "$verified_sha"
  save_stack
  [ -n "$run_url" ] || run_url="(run url not reported)"
  say "CI green: $run_url"
  if [ -n "$heartbeat_dead_note" ]; then
    say "$heartbeat_dead_note; landing its verified sha $verified_sha"
  fi
  land_verified_sha "$verified_sha" "$run_url"
  exit 0
fi

# ---------------------------------------------------------------------------
# Preconditions
# ---------------------------------------------------------------------------
# Step 0 reads the workflow files, because everything after it assumes a train
# push produces a run whose checks mean something.
#
# It is checked first because getting the order wrong is unrecoverable from the
# operator's side: turn on required status checks while the workflow still
# triggers on the default branch only, and every train pushes a branch that
# produces no run, so no check ever arrives and the branch becomes unpushable.
#
# The reading is done by scripts/lib/workflow-gates.py rather than by matching
# lines here: `if:` conditions are routinely written as folded scalars, and a
# line-oriented search misses them on exactly the workflows whose behaviour
# depends on the ref. The path derives from the same variable the probe and
# the watch use: two spellings of the workflow name in one script disagreed on
# every lift whose gate is not called tests.yml.
tests_workflow="$REPO/.github/workflows/$tests_workflow_name"
gate_scanner="$REPO/scripts/lib/workflow-gates.py"

command -v python3 >/dev/null 2>&1 ||
  refuse "python3 is required to read the workflow files"
if [ ! -f "$tests_workflow" ]; then
  # `ls` on an unmatched glob exits nonzero, which under `set -e` would end the
  # script inside this substitution before the refusal is printed.
  present="$({ find "$REPO/.github/workflows" -maxdepth 1 \( -name '*.yml' -o -name '*.yaml' \) 2>/dev/null || true; } | sort | tr '\n' ' ')"
  refuse "no $tests_workflow — set WATCH_CI_WORKFLOW=<file> to the workflow that gates a landing (present: ${present:-none}), and add \`train/**\` to on.push.branches in that file"
fi

set +e
gate_report="$(python3 "$gate_scanner" --train-ref "$train_ref" \
  --tests-workflow "$tests_workflow" \
  "$REPO"/.github/workflows/*.yml "$REPO"/.github/workflows/*.yaml 2>&1)"
scanner_rc=$?
set -e
if [ "$scanner_rc" -ne 0 ]; then
  printf '%s\n' "$gate_report" >&2
  refuse "could not read the workflow files (workflow-gates.py exit $scanner_rc)"
fi

# A here-string rather than `printf | grep -q`: under pipefail, grep -q closes
# the pipe on its first match and a producer still writing takes SIGPIPE, so
# the pipeline returns 141 and the guard reads FALSE exactly when the row is
# present. The report is small enough today that the writer usually finishes
# first, which is the kind of reasoning that hides the defect until it does not.
if ! grep -qx 'trigger|ok' <<<"$gate_report"; then
  refuse "tests.yml does not run on $train_ref — add \`train/**\` to on.push.branches in .github/workflows/tests.yml"
fi

# Conditions that decide on something the landing path never satisfies. A gate
# that never evaluates true is not a check that passed, it is a check that never
# ran - and in a run summary the two are indistinguishable.
#
#   ref conditions   - a job gated on the default branch's ref is skipped on the
#                      train push (the ref is refs/heads/train/...), and the
#                      train run is the one whose checks protection consults
#                      when the fast-forward asks to land that sha. The gate
#                      therefore never guards anything. This refuses.
#   event conditions - whether `github.event_name == 'pull_request'` ever fires
#                      depends on how this repo lands changes, which cannot be
#                      read off the YAML. This warns.
#   tag-only refs    - a condition that only names refs/tags/ belongs to the
#                      release event rather than the landing path, and widening
#                      it to trains would be wrong. Warned, never refused.
#   concurrency      - a group that is not per-ref means pushing a train cancels
#                      an in-flight run on another ref. Warned.
#
# The judgement is "this condition against this trigger list", so findings are
# printed under a header naming the file, the events that start it and its
# concurrency group.
event_gated="$(printf '%s\n' "$gate_report" | sed -n 's/^warn|//p')"
ref_gated="$(printf '%s\n' "$gate_report" | sed -n 's/^ref|//p')"

if [ -n "$event_gated" ]; then
  {
    printf 'train-push: WARNING — conditions that may never fire on the landing path:\n'
    printf '%s\n' "$event_gated" | sed 's/^/  /'
    printf '  judge this against how the repo lands; the scanner cannot know the landing path\n'
  } >&2
fi

if [ -n "$ref_gated" ]; then
  {
    printf 'train-push: refusing — ref-gated conditions that no train can satisfy:\n'
    printf '%s\n' "$ref_gated" | sed 's/^/  /'
    printf '  fix: gate on github.event_name or paths, or widen the ref condition to include refs/heads/train/\n'
  } >&2
  exit 2
fi

# Last part of step 0: a repo-local pre-push hook that runs the full gate turns
# the red-train loop into something you cannot use - every fix-and-repush pays
# the gate again, and even deleting the branch after a land runs the suite. We
# only report it: the hook may be doing something else entirely, and running it
# here to find out would be the very cost being warned about.
warn_repo_local_pre_push() {
  local configured
  local repo_local="$git_common_dir/hooks/pre-push"
  local hook
  local -a candidates

  candidates=("$repo_local")
  configured="$(git -C "$REPO" config --get core.hooksPath 2>/dev/null || true)"
  if [ -n "$configured" ]; then
    case "$configured" in
      /*) : ;;
      *) configured="$REPO/$configured" ;;
    esac
  fi
  if [ -n "$configured" ]; then
    case "$configured" in
      # AFT's managed dispatcher is not a gate: it chains to the repo-local
      # hook, which is already a candidate above. Its presence alone says
      # nothing about whether a gate runs. Matched by path shape rather than an
      # absolute location because the data and cache directories move with XDG
      # settings. The first pair is the older per-storage-root location; newer
      # releases share one content-addressed set under the AFT cache directory.
      */cortexkit/aft/git-hooks | */cortexkit/aft/git-hooks/*) : ;;
      */aft/git-hooks/*) : ;;
      *)
        if [ "$configured/pre-push" != "$repo_local" ]; then
          candidates+=("$configured/pre-push")
        fi
        ;;
    esac
  fi

  for hook in "${candidates[@]}"; do
    [ -f "$hook" ] && [ -x "$hook" ] || continue
    {
      printf 'train-push: WARNING — repo-local pre-push hook: %s\n' "$hook"
      printf '  verify it does not refuse pushes to train/** or ref deletions\n'
    } >&2
  done
}

warn_repo_local_pre_push

# Refuse a tree that is mid-merge/cherry-pick/rebase for the same reason
# gated-push.sh does: a conflicted tree can carry stale HEAD state, and here it
# would also push a sha that is not the change under test.
#
# A rebase in progress is the DIRECTORY (rebase-merge/ or rebase-apply/), which
# git creates on start and removes on finish or abort. REBASE_HEAD is a
# convenience ref that git does not always remove when a rebase completes, so
# on its own it is a fossil, not an operation: a checkout that finished a
# rebase weeks ago would otherwise refuse every train until someone deleted
# the file by hand. MERGE_HEAD and CHERRY_PICK_HEAD are cleaned up by git and
# stay authoritative.
for marker in CHERRY_PICK_HEAD MERGE_HEAD; do
  if [ -e "$git_dir/$marker" ]; then
    refuse "$marker present (unresolved git operation)"
  fi
done
for rebase_dir in rebase-merge rebase-apply; do
  if [ -d "$git_dir/$rebase_dir" ]; then
    refuse "$rebase_dir/ present (rebase in progress)"
  fi
done
if [ -e "$git_dir/REBASE_HEAD" ]; then
  say "note: REBASE_HEAD present with no rebase in progress (a finished rebase's leftover ref; ignored)"
fi

# Editors run cargo without --locked on save (rust-analyzer's check-on-save
# is the measured case), and in a repository that path-depends on sibling
# checkouts that rewrites Cargo.lock to whatever those checkouts happen to be
# at: the `version =` line of each source-less package moves and nothing
# else. That diff cannot be an intentional edit here, because pins advance
# through refresh-siblings-lock.sh, which moves siblings.lock in the same
# change. So a lone Cargo.lock drift of exactly that shape is restored and
# named rather than refused. Anything wider (a `source =` or `checksum =`
# line, a dependency list, any other dirty file) is still the operator's
# uncommitted work and stays a refusal. The gate's guarantee holds either
# way: CI tests the pushed commit, and the restored lock IS that commit's.
#
# Prints the drifted package names, one per line, when the working tree is
# dirty in exactly that way; prints nothing and returns 1 otherwise.
sibling_lock_drift_packages() {
  [ "$(git -C "$REPO" status --porcelain)" = " M Cargo.lock" ] || return 1
  # Every changed line must be a version line. -U1 keeps the `name =` line
  # that precedes `version =` in a [[package]] block as context.
  local diff non_version
  diff="$(git -C "$REPO" diff -U1 -- Cargo.lock)"
  # One awk pass rather than a grep pipeline ending in -q: under pipefail a
  # `grep -qv` that meets a non-version line first exits, the writers behind
  # it take SIGPIPE, the pipeline returns 141, and the `if` reads FALSE — so a
  # real working-tree change early in a large diff was classified as pure
  # drift and Cargo.lock was RESTORED over the operator's uncommitted work.
  # Reading to EOF and counting cannot be inverted that way.
  non_version="$(printf '%s\n' "$diff" | awk '
    /^(\+\+\+|---)/ { next }
    /^[-+]/ && !/^[-+]version = "/ { n++ }
    END { print n + 0 }')"
  if [ "$non_version" -ne 0 ]; then
    return 1
  fi
  # For each removed version line, find the [[package]] block in HEAD's lock
  # with that name and that version and require it to have no `source =`.
  # A name can appear twice (a path copy beside a registry or git copy of
  # the same crate), which is why the block is matched on both fields.
  local head_lock names name old
  head_lock="$(git -C "$REPO" show HEAD:Cargo.lock)"
  names=""
  while IFS= read -r line; do
    case "$line" in
      'name = "'*) name="${line#name = \"}"; name="${name%\"}" ;;
      '-version = "'*)
        old="${line#-version = \"}"; old="${old%\"}"
        [ -n "$name" ] || return 1
        printf '%s\n' "$head_lock" | awk -v n="$name" -v v="$old" '
          /^\[\[package\]\]/ { inblk=1; hit=0; src=0; next }
          inblk && $0 == "name = \"" n "\"" { hit=1; next }
          inblk && hit && $0 == "version = \"" v "\"" { ver=1; next }
          inblk && hit && ver && /^source = / { src=1 }
          inblk && /^$/ { if (hit && ver) { found=1; if (src) bad=1 } inblk=0; hit=0; ver=0; src=0 }
          END { if (hit && ver) { found=1; if (src) bad=1 } exit !(found && !bad) }
        ' || return 1
        names="${names}${name}\n"
        name=""
        ;;
    esac
  done <<EOF_DIFF
$(printf '%s\n' "$diff" | grep -E '^( name = "|-version = ")' | sed 's/^ //')
EOF_DIFF
  [ -n "$names" ] || return 1
  printf '%b' "$names"
}

# CI tests the pushed commit, not the working tree. Uncommitted work would be
# invisible to the gate and then silently absent from what lands on main.
if drifted="$(sibling_lock_drift_packages)"; then
  git -C "$REPO" checkout -- Cargo.lock
  say "restored Cargo.lock: sibling path-dependency drift in $(printf '%s' "$drifted" | paste -sd, -) (an editor's cargo run without --locked; the committed lock is what CI tests)"
fi
if [ -n "$(git -C "$REPO" status --porcelain)" ]; then
  # Name the paths: a tree that is dirty only for a moment (a tool writing
  # into the checkout) is otherwise invisible by the time anyone looks.
  git -C "$REPO" status --porcelain | head -20 >&2
  refuse "working tree is not clean (commit or stash before pushing a train)"
fi

if [ "$restack" -eq 1 ]; then
  restack_child
fi

# A train must not add local dependency paths that escape the repository. CI
# runs the same check, and this catches the problem before spending a push run.
# The checker ships beside this script; a repository that copied only
# train-push.sh and watch-ci.sh must be told that, not shown a path verdict.
if [ ! -f "$REPO/scripts/check-path-deps.py" ]; then
  refuse "scripts/check-path-deps.py not found: lift it from aft with train-push.sh, or provide your own check under that name"
fi
if ! python3 "$REPO/scripts/check-path-deps.py"; then
  refuse "dependency path resolves outside the repository"
fi

# .cortexkit/ is private CortexKit working space: Git may track only the paths
# the ignore rules re-include there. CI's Landing policy job runs
# scripts/check-cortexkit-tracking.sh; this block repeats its Git query inline
# (so train-push.sh needs no extra file) because this checkout's
# .git/info/exclude can ignore files that a fresh CI clone does not.
# The tree is clean at this point, so the index is exactly what gets pushed.
if ! cortexkit_tracked="$(git -C "$REPO" ls-files -ci --exclude-standard -- .cortexkit)"; then
  refuse "could not list tracked files under .cortexkit/"
fi
if [ -n "$cortexkit_tracked" ]; then
  printf '%s\n' "$cortexkit_tracked" | sed 's/^/  /' >&2
  refuse "ignored files are tracked under .cortexkit/ (stop tracking them with git rm --cached <path>, which keeps the working copy)"
fi

# A local main behind origin/main means an unstacked train was built on a stale
# base. A stacked worktree instead names its exact parent base, and --restack
# just replayed onto a fetched base; neither needs the idle local main moved.
if [ -z "$parent_name" ] && [ "$restack" -eq 0 ] && git -C "$REPO" rev-parse --verify -q refs/heads/"$default_branch" >/dev/null; then
  if ! git -C "$REPO" merge-base --is-ancestor "$remote_default" "refs/heads/$default_branch"; then
    refuse "local $default_branch is behind $remote/$default_branch (git merge --ff-only $remote/$default_branch first)"
  fi
fi

# HEAD is what lands, so HEAD is what has to fast-forward main. Checked here so
# a doomed train is refused before it costs a CI run, and again after CI.
if ! git -C "$REPO" merge-base --is-ancestor "$remote_default" "$head_sha"; then
  refuse "HEAD is not a descendant of $remote/$default_branch (rebase onto $remote/$default_branch first)"
fi

# ---------------------------------------------------------------------------
# First-run self-check: prove the trigger instead of believing the YAML
# ---------------------------------------------------------------------------
# The scan above reads intent; this reads the platform. A widened trigger that
# has never been exercised is a claim, so once per repository push a throwaway
# commit to a probe branch and wait for a run to START on it. If the two
# disagree, it is always the platform that is right.
probe_marker="$git_common_dir/train-push-proven"

run_trigger_probe() {
  local probe_ref="train/trigger-probe"
  local probe_sha
  local rid=""
  local _attempt

  say "first train in this repo — proving $tests_workflow_name starts on a train branch"
  # commit-tree writes the probe commit as a loose object: HEAD, the index and
  # the working tree are untouched by the probe.
  probe_sha="$(git -C "$REPO" commit-tree "$head_sha^{tree}" -p "$head_sha" -m "train-push trigger probe")" ||
    refuse "could not build the trigger probe commit"
  git -C "$REPO" push -q --force "$remote" "$probe_sha:refs/heads/$probe_ref" ||
    refuse "could not push the trigger probe to $remote/$probe_ref"

  # ~2 minutes. A run that is going to exist is queued within seconds; waiting
  # longer would only delay the first train in every clone.
  #
  # Resolved by SHA across every workflow rather than filtered by workflow
  # file: the forge lists workflow files from the default branch, so a file the
  # train itself adds or renames is not queryable under its new name until it
  # lands, and a file-filtered query found nothing for a probe whose run had
  # started (the first lift that renamed ci.yml hit exactly this). The run is
  # matched to the gating workflow by its display name, which the file carries
  # in `name:` and which survives a rename; a file without `name:` is shown
  # under its path, so that is the fallback expectation.
  local want_name
  want_name="$(sed -n 's/^name:[[:space:]]*//p' "$tests_workflow" | head -1 | sed 's/^["'"'"']//; s/["'"'"']$//')"
  [ -n "$want_name" ] || want_name="$tests_workflow"
  for _attempt in $(seq 1 "$probe_attempts"); do
    rid="$(gh_query_or_refuse "trigger-probe CI runs for $probe_sha" "$OPERATOR_GH" run list \
      --repo "$repo_slug" \
      --branch "$probe_ref" --limit 20 --json databaseId,headSha,workflowName \
      --jq ".[] | select(.headSha==\"$probe_sha\" and .workflowName==\"$want_name\") | .databaseId")" || return "$?"
    [ -n "$rid" ] && break
    sleep "$probe_sleep"
  done

  local delete_error
  if ! delete_error="$(git -C "$REPO" push -q "$remote" --delete "$probe_ref" 2>&1)"; then
    printf 'train-push: warning — could not delete %s/%s: %s\n' "$remote" "$probe_ref" "$delete_error" >&2
  fi

  if [ -z "$rid" ]; then
    refuse "no $tests_workflow_name run started for the probe on $probe_ref — the platform does not run it on train branches, whatever the workflow file says"
  fi

  printf 'run %s started for probe %s on %s at %s\n' \
    "$rid" "$probe_sha" "$probe_ref" "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" > "$probe_marker"
  say "trigger proven by run $rid"
}

if [ ! -f "$probe_marker" ]; then
  run_trigger_probe
fi

# Repo-local preflights. The script is lifted into other repositories as a
# file, so nothing here may assume this repository's layout: each preflight
# runs only when its script exists, and the header line names which ones ran
# so a repository with none is distinguishable from one whose block was
# deleted. Publication inputs are checked in a disposable snapshot: no
# generated document, alignment commit, or native release build is needed.
preflights_ran=()
if [ -f "$REPO/scripts/release-gate-v049.mjs" ]; then
  node "$REPO/scripts/release-gate-v049.mjs" --check-inputs \
    || refuse "publication input check failed — fix the source, schema, policy, or reviewed vocabulary allowlist"
  preflights_ran+=(publication-inputs)
fi
# A repository may add its own preflights beside this script without editing
# it; the hook is sourced so it can call `refuse` and `say`.
if [ -f "$REPO/scripts/train-push.local.sh" ]; then
  # shellcheck disable=SC1091
  . "$REPO/scripts/train-push.local.sh"
  preflights_ran+=(train-push.local.sh)
fi
if [ "${#preflights_ran[@]}" -eq 0 ]; then
  say "preflights: none (no repo-local preflight scripts present)"
else
  say "preflights: ${preflights_ran[*]}"
fi

# ---------------------------------------------------------------------------
# Optional local smoke
# ---------------------------------------------------------------------------
if [ "$smoke_given" -eq 1 ]; then
  say "local smoke: ${smoke_cmd[*]}"
  set +e
  if [ "${#smoke_cmd[@]}" -eq 1 ]; then
    # A single argument is a shell string, which may be a pipeline. Without
    # pipefail a failing first stage is hidden behind a successful last stage,
    # so the smoke would report green over a failed command.
    (cd "$REPO" && bash -c "set -o pipefail
${smoke_cmd[0]}")
  else
    # Argv form runs bare: this script adds no pipe of its own, so the
    # command's own status is the status we read.
    (cd "$REPO" && "${smoke_cmd[@]}")
  fi
  smoke_rc=$?
  set -e
  if [ "$smoke_rc" -ne 0 ]; then
    refuse "local smoke failed (rc=$smoke_rc): ${smoke_cmd[*]}"
  fi
  say "local smoke ok"
fi

# ---------------------------------------------------------------------------
# Push the train, let CI gate it, land on green - re-queue if main moved
# ---------------------------------------------------------------------------
# main can move during the ~11-16 minutes CI takes, and a sha tested on the old
# base says nothing about the rebased result. A moved main is therefore a new
# round rather than a hand-back: rebase onto the new main, re-push the branch,
# watch a fresh run. Bounded at 3 because retrying forever on a busy main is
# unbounded CI spend, and by the third loss the honest answer is that this train
# needs a quiet window.
max_rounds=3
round=1
# The sha a check actually ran green against. Only this sha may land.
verified_sha=""

# What we believe $remote/$train_ref points at. Tracked explicitly so every
# re-push leases against the sha WE pushed instead of trusting a remote-tracking
# ref to have been refreshed along the way.

push_train() {
  save_stack
  if [ -z "$train_remote_sha" ]; then
    # Create with a plain push: a mistyped train name must not be able to
    # clobber a branch that already exists.
    say "creating $remote/$train_ref -> $head_sha"
    git -C "$REPO" push "$remote" "$head_sha:refs/heads/$train_ref" ||
      refuse "could not create $remote/$train_ref"
  else
    say "updating $remote/$train_ref -> $head_sha"
    git -C "$REPO" push --force-with-lease="refs/heads/$train_ref:$train_remote_sha" \
      "$remote" "$head_sha:refs/heads/$train_ref" ||
      refuse "could not update $remote/$train_ref (it moved since we pushed it — check who else is running this train)"
  fi
  train_remote_sha="$head_sha"
}

# Rebase the checked-out train onto the new main. A conflict ends the run: the
# resolution is a human judgement about two changes, and leaving a half-rebased
# tree behind would hand back a repository that cannot be used until someone
# figures out what state it is in.
rebase_onto_main() {
  local before="$1"
  local conflicted
  local now
  local merges

  # A plain rebase linearizes: it replays the non-merge commits and drops the
  # merge commit itself, so anything recorded only in that merge (a conflict
  # resolution, an integration fix) vanishes with exit 0 and the reduced tree
  # is what gets re-tested and landed. --rebase-merges does not help: it
  # re-performs the merges and loses the same adjustment unless rerere holds
  # it. A merge is a permitted train shape (see the header), so refuse the
  # automatic requeue and leave the remote train ref intact for the operator,
  # who knows what the merge resolved. (BROCA's find, 2026-09-11.)
  merges="$(git -C "$REPO" rev-list --merges "$remote_default..$before" 2>/dev/null || true)"
  if [ -n "$merges" ]; then
    {
      printf 'train-push: %s/%s moved and the train carries merge commit(s) — not rebasing.\n' \
        "$remote" "$default_branch"
      printf '  A rebase would drop the merge and anything recorded only in it.\n'
      printf '%s\n' "$merges" | sed 's/^/    merge /'
      printf '  %s/%s still holds %s. Integrate %s/%s yourself (merge or re-pick),\n' \
        "$remote" "$train_ref" "$before" "$remote" "$default_branch"
      printf '  then re-run: scripts/train-push.sh %s\n' "$train_name"
    } >&2
    return 1
  fi

  if git -C "$REPO" rebase "$remote_default"; then
    return 0
  fi

  conflicted="$(git -C "$REPO" diff --name-only --diff-filter=U 2>/dev/null || true)"
  git -C "$REPO" rebase --abort >/dev/null 2>&1 || true
  now="$(git -C "$REPO" rev-parse HEAD)"
  {
    printf 'train-push: rebase onto %s/%s conflicted — stopping.\n' "$remote" "$default_branch"
    if [ -n "$conflicted" ]; then
      printf '  conflicting file(s):\n'
      printf '%s\n' "$conflicted" | sed 's/^/    /'
    fi
    if [ "$now" = "$before" ]; then
      printf '  The rebase was aborted; the tree is back at %s and %s/%s still holds it.\n' \
        "$before" "$remote" "$train_ref"
    else
      printf '  WARNING: the abort did not restore HEAD (now %s, was %s) — check the tree before continuing.\n' \
        "$now" "$before"
    fi
    printf '  Resolve by hand, then run: scripts/train-push.sh %s\n' "$train_name"
  } >&2
  return 1
}

while true; do
  push_train

  verified_sha=""
  say "watching CI for $head_sha on $train_ref in $repo_slug (round $round of $max_rounds)"
  set +e
  (cd "$REPO" && REPO="$repo_slug" WATCH_CI_BRANCH="$train_ref" WATCH_CI_HEARTBEAT="$watch_heartbeat" \
    TRAIN_PUSH_QUEUED_LIMIT_MIN="${TRAIN_PUSH_QUEUED_LIMIT_MIN:-10}" \
    "$script_dir/watch-ci.sh" "$head_sha") 2>&1 | tee "$watch_log"
  watch_rc="${PIPESTATUS[0]}"
  set -e

  run_url="$(grep -m1 '^CI_RUN_URL ' "$watch_log" 2>/dev/null | sed 's/^CI_RUN_URL //' || true)"
  [ -n "$run_url" ] || run_url="(run url not reported)"

  if [ "$watch_rc" -ne 0 ]; then
    if [ "$watch_rc" -eq 1 ]; then
      # Report the jobs by name: with three platforms in parallel, "CI failed"
      # is not enough to know whether this needs a local reproduction or a
      # platform-specific fix. The watch's early-fail line carries only the
      # name, so the job id is read back from the run's jobs listing: the skew
      # check needs it for the failing step names and the log tail.
      run_id="$(grep -o 'run=[0-9]\+' "$watch_log" 2>/dev/null | head -1 | sed 's/^run=//' || true)"
      if [ -z "$run_id" ]; then
        run_id="$(resolve_ci_run "$head_sha")" || exit "$?"
      fi
      failing_pairs=""
      if [ -n "$run_id" ]; then
        failing_pairs="$(gh_query_or_refuse "failed jobs for CI run $run_id" "$OPERATOR_GH" \
          run view "$run_id" --repo "$repo_slug" --json jobs \
          --jq '[.jobs[] | select(.conclusion=="failure") | .name + "|" + (.databaseId|tostring)] | .[]')" || exit "$?"
      fi
      failing="$(printf '%s\n' "$failing_pairs" | sed 's/|.*//' | sed '/^$/d' || true)"
      if [ -n "$failing" ]; then
        printf 'train-push: CI red — failing job(s):\n' >&2
        # One name per line, not word-split: job names contain spaces.
        printf '%s\n' "$failing" | sed 's/^/  /' >&2
      else
        printf 'train-push: CI red — no job name reported (run conclusion was not success)\n' >&2
      fi
      # A red run never enters the re-queue loop below - that loop is for a
      # moved default branch and nothing else. Rebasing a red train carries the
      # same tree onto a new base, so the re-run is red for the same reason and
      # spends one of the bounded rounds proving it. Dependency skew gets named
      # because it looks like a flake from the outside: the commit did not
      # change, CI resolved sibling repositories to different versions, and no
      # amount of retrying moves a lockfile - the fix is a bump commit.
      if red_is_skew "$run_id" "$failing_pairs"; then
        printf 'red is a version/lock skew, not contention: this terminates in a lockfile bump commit, not a retry\n' >&2
      else
        skew_rc=$?
        [ "$skew_rc" -eq 1 ] || exit "$skew_rc"
      fi
      printf 'train-push: run %s\n' "$run_url" >&2
      printf 'train-push: %s/%s still holds %s — fix, commit, and re-run: scripts/train-push.sh %s\n' \
        "$remote" "$train_ref" "$head_sha" "$train_name" >&2
      exit 1
    fi
    printf 'train-push: could not watch CI for %s (watch-ci exit %s); %s/%s is pushed and unwatched\n' \
      "$head_sha" "$watch_rc" "$remote" "$train_ref" >&2
    exit 2
  fi

  require_ci_success "$head_sha"
  verified_sha="$head_sha"
  say "CI green: $run_url"

  if [ -n "$parent_name" ]; then
    if wait_for_parent "$verified_sha" "$run_url"; then
      break
    else
      parent_wait_rc=$?
      exit "$parent_wait_rc"
    fi
  fi

  # Re-check right before the push, not just at the start of the script.
  require_repo_after_green "$head_sha" "$run_url"
  fetch_remote -q "$remote" "$default_branch" ||
    landing_failed "git fetch $remote $default_branch failed" "$head_sha" "$run_url"
  if git -C "$REPO" merge-base --is-ancestor "$remote_default" "$head_sha"; then
    break
  fi

  moved_sha="$(git -C "$REPO" rev-parse "$remote_default")"
  printf 'train-push: %s/%s moved to %s while CI ran (round %s of %s) — %s was tested on the old base\n' \
    "$remote" "$default_branch" "$moved_sha" "$round" "$max_rounds" "$head_sha" >&2

  if [ "$round" -ge "$max_rounds" ]; then
    {
      printf 'train-push: gave up after %s rounds — not landing.\n' "$max_rounds"
      printf '  %s/%s holds %s, rebased onto every %s this run saw.\n' \
        "$remote" "$train_ref" "$head_sha" "$remote/$default_branch"
      printf '  Re-run when %s is quieter: scripts/train-push.sh %s\n' \
        "$remote/$default_branch" "$train_name"
    } >&2
    exit 3
  fi

  if ! rebase_onto_main "$head_sha"; then
    exit 3
  fi
  head_sha="$(git -C "$REPO" rev-parse HEAD)"
  round=$((round + 1))
  say "rebased onto $moved_sha — train head is now $head_sha"
done

# The invariant, enforced rather than assumed: what lands is the sha the check
# ran against. If any future edit moves HEAD between the watch and this push,
# this stops main from fast-forwarding to a commit nothing verified.
if [ "$verified_sha" != "$head_sha" ]; then
  landing_failed "refusing to land $head_sha because the green check ran against ${verified_sha:-nothing}" \
    "$head_sha" "$run_url"
fi

# A final stack check covers a default-branch move after the parent wait. The
# push itself stays non-force, so a concurrent divergent landing is refused.
if [ -n "$parent_name" ]; then
  wait_for_parent "$verified_sha" "$run_url"
fi
wait_for_required_status_checks "$head_sha" "$run_url"
say "landing $head_sha on $remote/$default_branch"
set +e
git -C "$REPO" push "$remote" "$head_sha:refs/heads/$default_branch" 2>&1 | tee "$push_log"
land_rc="${PIPESTATUS[0]}"
set -e
if [ "$land_rc" -ne 0 ]; then
  # Branch protection rejecting the sha for want of a check is the one push
  # failure with a specific remedy, so say what it is instead of leaving the
  # operator to decode GH006.
  land_reason="push of $head_sha to $remote/$default_branch failed; $remote/$train_ref still holds the tested sha"
  if grep -qE 'GH006|equired status check|rotected branch update failed' "$push_log"; then
    land_reason="$(required_check_failure_reason "$head_sha"); merge onto the train branch and push there; CI runs on the merge sha, then main fast-forwards."
  fi
  landing_failed "$land_reason" "$head_sha" "$run_url"
fi

# Outcome check, not just command check: a push can report success through a
# wrapper (or fail on auth) while origin never moved.
fetch_remote -q "$remote" "$default_branch" ||
  landing_failed "could not verify $remote/$default_branch after push" "$head_sha" "$run_url"
if ! git -C "$REPO" merge-base --is-ancestor "$head_sha" "$remote_default"; then
  landing_failed "push reported success but $head_sha is not on $remote/$default_branch" "$head_sha" "$run_url"
fi
say "landed $head_sha on $remote/$default_branch"

# The branch existed to carry the train through CI; once the sha is on main it
# is noise. A failed delete does not un-land the commit, so it is a warning.
if ! git -C "$REPO" push -q "$remote" --delete "$train_ref"; then
  printf 'train-push: warning — could not delete %s/%s (delete it by hand)\n' "$remote" "$train_ref" >&2
fi
rm -f "$watch_heartbeat"
say "done"
