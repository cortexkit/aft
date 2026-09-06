#!/usr/bin/env bash
# Push a train to its own branch, let CI be the gate, and advance main only on
# green. Operator tooling runs through the real GitHub CLI (gh) via watch-ci.sh;
# the shim is only for AI agent commands.
#
# Lifting this into another repository: carry four files together —
# scripts/train-push.sh, scripts/watch-ci.sh, scripts/lib/operator-gh.sh,
# scripts/lib/workflow-gates.py — plus `python3`, `git`, and the real `gh` on
# PATH. Nothing else here assumes this repository's layout: the default branch
# is read from origin/HEAD, and repo-local preflights run only when their
# scripts exist (see "Repo-local preflights" below), with the header line
# naming which ones ran.
#
# Why this is the default push path instead of scripts/gated-push.sh:
# the local full Rust gate takes ~12 minutes on this box and only sees macOS.
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
# locally (watcher/fseventsd, macOS exec assessment).
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
#   scripts/train-push.sh <train-name> -- <local smoke command...>
#
# The optional smoke is the targeted slice the diff touches (the one or two
# suites you would rerun by hand), NOT the full gate. It exists to catch an
# obvious break before spending a CI run.
#
# A red train leaves origin/train/<train-name> in place: fix, commit, and run
# this script again with the same name to update the branch and re-run CI.
#
# If main moves while CI runs, the train is re-queued automatically: rebase onto
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
#   2  precondition refusal, bad usage, failed smoke, or no CI run resolved
#   3  the default branch kept moving through 3 re-queue rounds, or the rebase
#      conflicted
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

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
  url="$(git config --get "remote.$remote.url" 2>/dev/null)" || return 1
  case "$url" in
    git@github.com:*) url="${url#git@github.com:}" ;;
    https://github.com/*) url="${url#https://github.com/}" ;;
    ssh://git@github.com/*) url="${url#ssh://git@github.com/}" ;;
    *) return 1 ;;
  esac
  printf '%s\n' "${url%.git}"
}
repo_slug="${REPO:-}"
# How long the first-run probe waits for a run to start. Overridable for the
# same reason watch-ci.sh's resolver knobs are: tests cannot wait out the
# real budget.
probe_attempts="${TRAIN_PUSH_PROBE_ATTEMPTS:-12}"
probe_sleep="${TRAIN_PUSH_PROBE_SLEEP:-10}"
# Failing job names that mean the red is dependency skew rather than a broken
# change: in this repo those are the Cargo.lock and manifest checks. Extend it
# when a repo names such a job something else.
skew_pattern="${TRAIN_PUSH_SKEW_PATTERN:-}"
if [ -z "$skew_pattern" ]; then
  skew_pattern='lock|version|pin|sibling'
fi

say() { printf 'train-push: %s\n' "$1"; }
refuse() {
  printf 'train-push: refusing — %s\n' "$1" >&2
  exit 2
}

# ---------------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------------
if [ "$#" -eq 0 ]; then
  refuse "no train name given (usage: scripts/train-push.sh <train-name> [-- <smoke command>])"
fi

train_name="$1"
shift
case "$train_name" in
  -*) refuse "train name must not look like a flag (got '$train_name')" ;;
esac

smoke_given=0
smoke_cmd=()
if [ "$#" -gt 0 ]; then
  if [ "$1" != "--" ]; then
    refuse "unexpected argument '$1' (the smoke command must follow a bare --)"
  fi
  shift
  if [ "$#" -eq 0 ]; then
    refuse "-- given without a smoke command"
  fi
  smoke_given=1
  smoke_cmd=("$@")
fi

train_ref="train/$train_name"
# Validate before the name reaches a refspec: a name with a space, a leading
# dot, or '..' in it produces a confusing git error deep in the push instead of
# a named refusal here.
if ! git check-ref-format "refs/heads/$train_ref"; then
  refuse "'$train_name' is not a usable branch name component"
fi

# Operator tooling runs on the upstream gh; the shim is only for agent commands.
# Sourced up front because the trigger probe, the default-branch fallback, and
# watch-ci.sh all need it - failing here beats failing after a CI run.
# shellcheck source=lib/operator-gh.sh
source "$script_dir/lib/operator-gh.sh" || exit 2

if [ -z "$repo_slug" ]; then
  repo_slug="$(repo_from_origin)" ||
    refuse "cannot derive the repository from $remote's URL; set REPO=owner/name"
fi
# watch-ci.sh derives the same value the same way; exporting it pins the watch
# to the repository this script proved the trigger on.
export REPO="$repo_slug"

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
tests_workflow=".github/workflows/$tests_workflow_name"
gate_scanner="$script_dir/lib/workflow-gates.py"
# Absolute, so the pre-push warning below names a path the reader can act on
# from anywhere rather than one relative to the repo root.
git_dir="$(git rev-parse --absolute-git-dir)"

command -v python3 >/dev/null 2>&1 ||
  refuse "python3 is required to read the workflow files"
if [ ! -f "$tests_workflow" ]; then
  # `ls` on an unmatched glob exits nonzero, which under `set -e` would end the
  # script inside this substitution before the refusal is printed.
  present="$({ find .github/workflows -maxdepth 1 \( -name '*.yml' -o -name '*.yaml' \) 2>/dev/null || true; } | sort | tr '\n' ' ')"
  refuse "no $tests_workflow — set WATCH_CI_WORKFLOW=<file> to the workflow that gates a landing (present: ${present:-none}), and add \`train/**\` to on.push.branches in that file"
fi

set +e
gate_report="$(python3 "$gate_scanner" --train-ref "$train_ref" \
  --tests-workflow "$tests_workflow" \
  .github/workflows/*.yml .github/workflows/*.yaml 2>&1)"
scanner_rc=$?
set -e
if [ "$scanner_rc" -ne 0 ]; then
  printf '%s\n' "$gate_report" >&2
  refuse "could not read the workflow files (workflow-gates.py exit $scanner_rc)"
fi

if ! printf '%s\n' "$gate_report" | grep -qx 'trigger|ok'; then
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
  local repo_local="$git_dir/hooks/pre-push"
  local hook
  local -a candidates

  candidates=("$repo_local")
  configured="$(git config --get core.hooksPath 2>/dev/null || true)"
  if [ -n "$configured" ]; then
    case "$configured" in
      # AFT's managed dispatcher is not a gate: it chains to the repo-local
      # hook, which is already a candidate above. Its presence alone says
      # nothing about whether a gate runs. Matched by path shape rather than an
      # absolute location because the data directory moves with XDG settings.
      */cortexkit/aft/git-hooks | */cortexkit/aft/git-hooks/*) : ;;
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
for marker in CHERRY_PICK_HEAD MERGE_HEAD REBASE_HEAD; do
  if [ -e "$git_dir/$marker" ]; then
    refuse "$marker present (unresolved git operation)"
  fi
done

# CI tests the pushed commit, not the working tree. Uncommitted work would be
# invisible to the gate and then silently absent from what lands on main.
if [ -n "$(git status --porcelain)" ]; then
  refuse "working tree is not clean (commit or stash before pushing a train)"
fi

git fetch -q --prune "$remote" || refuse "git fetch $remote failed"

# Read the default branch off the remote. Every later message, the moved-branch
# check and the fast-forward target all come from this one answer, so a repo
# whose default is not called main is landed on correctly instead of silently
# having a 'main' created for it.
resolve_default_branch() {
  local ref
  local name

  ref="$(git symbolic-ref -q "refs/remotes/$remote/HEAD" 2>/dev/null || true)"
  name="${ref#refs/remotes/"$remote"/}"
  if [ -n "$ref" ] && [ -n "$name" ] && [ "$name" != "$ref" ]; then
    printf '%s\n' "$name"
    return 0
  fi

  # A remote wired up by hand has no origin/HEAD to read; ask the forge rather
  # than guessing a name.
  name="$("$OPERATOR_GH" repo view "$repo_slug" --json defaultBranchRef --jq '.defaultBranchRef.name' 2>/dev/null || true)"
  [ -n "$name" ] || return 1
  printf '%s\n' "$name"
}

default_branch="$(resolve_default_branch || true)"
if [ -z "$default_branch" ]; then
  refuse "could not determine $remote's default branch (run: git remote set-head $remote -a)"
fi

remote_default="refs/remotes/$remote/$default_branch"
if ! git rev-parse --verify -q "$remote_default" >/dev/null; then
  refuse "no $remote/$default_branch to land on"
fi

# A local main behind origin/main means the train was built on a stale base:
# CI would test it green and the land would still be refused in step 5.
if git rev-parse --verify -q refs/heads/"$default_branch" >/dev/null; then
  if ! git merge-base --is-ancestor "$remote_default" "refs/heads/$default_branch"; then
    refuse "local $default_branch is behind $remote/$default_branch (git merge --ff-only $remote/$default_branch first)"
  fi
fi

head_sha="$(git rev-parse HEAD)"
# HEAD is what lands, so HEAD is what has to fast-forward main. Checked here so
# a doomed train is refused before it costs a CI run, and again after CI.
if ! git merge-base --is-ancestor "$remote_default" "$head_sha"; then
  refuse "HEAD is not a descendant of $remote/$default_branch (rebase onto $remote/$default_branch first)"
fi

# ---------------------------------------------------------------------------
# First-run self-check: prove the trigger instead of believing the YAML
# ---------------------------------------------------------------------------
# The scan above reads intent; this reads the platform. A widened trigger that
# has never been exercised is a claim, so once per repository push a throwaway
# commit to a probe branch and wait for a run to START on it. If the two
# disagree, it is always the platform that is right.
probe_marker="$git_dir/train-push-proven"

run_trigger_probe() {
  local probe_ref="train/trigger-probe"
  local probe_sha
  local rid=""
  local _attempt

  say "first train in this repo — proving $tests_workflow_name starts on a train branch"
  # commit-tree writes the probe commit as a loose object: HEAD, the index and
  # the working tree are untouched by the probe.
  probe_sha="$(git commit-tree "$head_sha^{tree}" -p "$head_sha" -m "train-push trigger probe")" ||
    refuse "could not build the trigger probe commit"
  git push -q --force "$remote" "$probe_sha:refs/heads/$probe_ref" ||
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
    rid="$("$OPERATOR_GH" run list --repo "$repo_slug" \
      --branch "$probe_ref" --limit 20 --json databaseId,headSha,workflowName \
      --jq ".[] | select(.headSha==\"$probe_sha\" and .workflowName==\"$want_name\") | .databaseId" 2>/dev/null | head -1)"
    [ -n "$rid" ] && break
    sleep "$probe_sleep"
  done

  git push -q "$remote" --delete "$probe_ref" ||
    printf 'train-push: warning — could not delete %s/%s\n' "$remote" "$probe_ref" >&2

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
# deleted. This repository's two are the governed-docs gates gated-push.sh
# runs; both are read-only here (scripts/align-governed-docs.sh is the
# writing half and stays the remedy, not something this script performs).
preflights_ran=()
if [ -f scripts/audit-v049-agent-surface.ts ]; then
  bun scripts/audit-v049-agent-surface.ts \
    || refuse "governed-surface audit failed — run scripts/align-governed-docs.sh"
  preflights_ran+=(governed-surface-audit)
fi
if [ -f scripts/release-gate-v049.mjs ]; then
  node scripts/release-gate-v049.mjs \
    || refuse "release gate failed — run scripts/align-governed-docs.sh"
  preflights_ran+=(release-gate)
fi
# A repository may add its own preflights beside this script without editing
# it; the hook is sourced so it can call `refuse` and `say`.
if [ -f scripts/train-push.local.sh ]; then
  # shellcheck disable=SC1091
  . scripts/train-push.local.sh
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
    bash -c "set -o pipefail
${smoke_cmd[0]}"
  else
    # Argv form runs bare: this script adds no pipe of its own, so the
    # command's own status is the status we read.
    "${smoke_cmd[@]}"
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

watch_log="$(mktemp "${TMPDIR:-/tmp}/train-push-watch.XXXXXX")"
push_log="$(mktemp "${TMPDIR:-/tmp}/train-push-push.XXXXXX")"
trap 'rm -f "$watch_log" "$push_log"' EXIT

# What we believe $remote/$train_ref points at. Tracked explicitly so every
# re-push leases against the sha WE pushed instead of trusting a remote-tracking
# ref to have been refreshed along the way.
train_remote_sha=""
if git rev-parse --verify -q "refs/remotes/$remote/$train_ref" >/dev/null; then
  train_remote_sha="$(git rev-parse "refs/remotes/$remote/$train_ref")"
fi

push_train() {
  if [ -z "$train_remote_sha" ]; then
    # Create with a plain push: a mistyped train name must not be able to
    # clobber a branch that already exists.
    say "creating $remote/$train_ref -> $head_sha"
    git push "$remote" "$head_sha:refs/heads/$train_ref" ||
      refuse "could not create $remote/$train_ref"
  else
    say "updating $remote/$train_ref -> $head_sha"
    git push --force-with-lease="refs/heads/$train_ref:$train_remote_sha" \
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

  if git rebase "$remote_default"; then
    return 0
  fi

  conflicted="$(git diff --name-only --diff-filter=U 2>/dev/null || true)"
  git rebase --abort >/dev/null 2>&1 || true
  now="$(git rev-parse HEAD)"
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
  "$script_dir/watch-ci.sh" "$head_sha" 2>&1 | tee "$watch_log"
  watch_rc="${PIPESTATUS[0]}"
  set -e

  run_url="$(grep -m1 '^CI_RUN_URL ' "$watch_log" 2>/dev/null | sed 's/^CI_RUN_URL //' || true)"
  [ -n "$run_url" ] || run_url="(run url not reported)"

  if [ "$watch_rc" -ne 0 ]; then
    if [ "$watch_rc" -eq 1 ]; then
      # Report the jobs by name: with three platforms in parallel, "CI failed"
      # is not enough to know whether this needs a local reproduction or a
      # platform-specific fix.
      failing="$(grep -o "job='[^']*'" "$watch_log" 2>/dev/null | sed "s/job='//; s/'$//" | sort -u || true)"
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
      if [ -n "$failing" ] && printf '%s\n' "$failing" | grep -Eqi "$skew_pattern"; then
        printf 'red is a version/lock skew, not contention: this terminates in a lockfile bump commit, not a retry\n' >&2
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

  verified_sha="$head_sha"
  say "CI green: $run_url"

  # Re-check right before the push, not just at the start of the script.
  git fetch -q "$remote" "$default_branch" || refuse "git fetch $remote $default_branch failed"
  if git merge-base --is-ancestor "$remote_default" "$head_sha"; then
    break
  fi

  moved_sha="$(git rev-parse "$remote_default")"
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
  head_sha="$(git rev-parse HEAD)"
  round=$((round + 1))
  say "rebased onto $moved_sha — train head is now $head_sha"
done

# The invariant, enforced rather than assumed: what lands is the sha the check
# ran against. If any future edit moves HEAD between the watch and this push,
# this stops main from fast-forwarding to a commit nothing verified.
if [ "$verified_sha" != "$head_sha" ]; then
  printf 'train-push: refusing to land %s — the green check ran against %s\n' \
    "$head_sha" "${verified_sha:-nothing}" >&2
  exit 1
fi

say "landing $head_sha on $remote/$default_branch"
set +e
git push "$remote" "$head_sha:refs/heads/$default_branch" 2>&1 | tee "$push_log"
land_rc="${PIPESTATUS[0]}"
set -e
if [ "$land_rc" -ne 0 ]; then
  # Branch protection rejecting the sha for want of a check is the one push
  # failure with a specific remedy, so say what it is instead of leaving the
  # operator to decode GH006.
  if grep -qE 'GH006|equired status check|rotected branch update failed' "$push_log"; then
    printf 'refused: %s has no status check on origin. Merge onto the train branch and push there; CI runs on the merge sha, then main fast-forwards.\n' \
      "$head_sha" >&2
  fi
  printf 'train-push: push of %s to %s/%s failed; %s/%s still holds the tested sha\n' \
    "$head_sha" "$remote" "$default_branch" "$remote" "$train_ref" >&2
  exit 1
fi

# Outcome check, not just command check: a push can report success through a
# wrapper (or fail on auth) while origin never moved.
git fetch -q "$remote" "$default_branch"
if ! git merge-base --is-ancestor "$head_sha" "$remote_default"; then
  printf 'train-push: push reported success but %s is not on %s/%s — origin did not move\n' \
    "$head_sha" "$remote" "$default_branch" >&2
  exit 1
fi
say "landed $head_sha on $remote/$default_branch"

# The branch existed to carry the train through CI; once the sha is on main it
# is noise. A failed delete does not un-land the commit, so it is a warning.
if ! git push -q "$remote" --delete "$train_ref"; then
  printf 'train-push: warning — could not delete %s/%s (delete it by hand)\n' "$remote" "$train_ref" >&2
fi
say "done"
