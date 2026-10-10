#!/usr/bin/env bash
# Release CI gate: publish a tag only if tests.yml already passed on its commit.
#
# Every commit reaches main through scripts/train-push.sh, which fast-forwards
# main only after the tests.yml run on the commit's train branch went green, and
# main's branch protection requires those checks. Rerunning that matrix when the
# tag is pushed tests the same commit a second time and adds nothing but flake
# exposure, so release.yml runs this gate instead. It proves three things about
# the commit the tag points at:
#
#   1. it is on main: an ancestor of, or equal to, origin/main;
#   2. a completed tests.yml run for exactly this commit, started by a push to
#      main or to a train/* branch, concluded success. GitHub reports the latest
#      attempt, so a run whose failed jobs were rerun green counts as green;
#   3. every required status check of main is a job of that run and concluded
#      success. A job marked continue-on-error can fail inside a run that still
#      concludes success, so (2) alone does not cover the required set.
#
# The required set is read from main's branch protection when the token can
# read it. The release workflow's GITHUB_TOKEN cannot (that needs repository
# administration access), so there it comes from the committed list in
# .github/required-checks.txt, which must be kept in step with protection; a
# run with a token that can read both reports any difference.
#
# There is no skip switch. A refused tag is released by getting a green
# tests.yml run on its commit (rerun the failed jobs) and then rerunning the
# release workflow.
#
# Usage: scripts/release-ci-gate.sh <commit-ish>
# Needs git, an authenticated gh (GH_TOKEN in Actions), origin/main fetched
# locally, and the repository as GITHUB_REPOSITORY (set in Actions) or an
# origin remote on github.com.
# Exit: 0 releasable, 1 refused, 2 usage error.
set -euo pipefail

workflow="tests.yml"
main_branch="main"
main_ref="origin/$main_branch"
required_list_rel=".github/required-checks.txt"

say() { printf 'release-ci-gate: %s\n' "$*"; }

refuse() {
  if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
    printf '::error title=release-ci-gate::%s\n' "$*"
  fi
  printf 'release-ci-gate: REFUSED: %s\n' "$*" >&2
  exit 1
}

if [ $# -ne 1 ] || [ -z "$1" ]; then
  printf 'usage: %s <commit-ish>\n' "$0" >&2
  exit 2
fi

repo="${GITHUB_REPOSITORY:-}"
if [ -z "$repo" ]; then
  url="$(git config --get remote.origin.url 2>/dev/null || true)"
  case "$url" in
    git@github.com:*) repo="${url#git@github.com:}" ;;
    https://github.com/*) repo="${url#https://github.com/}" ;;
    ssh://git@github.com/*) repo="${url#ssh://git@github.com/}" ;;
  esac
  repo="${repo%.git}"
fi
[ -n "$repo" ] || refuse "cannot tell which repository to query: set GITHUB_REPOSITORY=owner/name"

# ^{commit} peels an annotated tag to the commit it points at, so the sha
# compared below is the one CI ran against whatever form the caller passes.
sha="$(git rev-parse --verify --quiet "$1^{commit}")" || refuse "cannot resolve '$1' to a commit"
say "commit $sha ($1) in $repo"

# --- 1. The commit is on main ----------------------------------------------
git rev-parse --verify --quiet "$main_ref^{commit}" >/dev/null ||
  refuse "$main_ref is not available locally; fetch $main_branch before running the gate"
if ! git merge-base --is-ancestor "$sha" "$main_ref"; then
  refuse "commit $sha is not on $main_branch (not an ancestor of $main_ref); only a commit that landed on $main_branch can be released"
fi
say "on $main_branch: yes ($main_ref is $(git rev-parse "$main_ref"))"

# --- 2. A green tests.yml push run for exactly this commit -------------------
# The server filters by commit too; the filter is repeated here so a response
# that ignores it can never let another commit's run through. Only push runs on
# main or a train branch count: a workflow_dispatch run of tests.yml can test a
# different ref than its own head sha, and a pull request run's branch is not
# one train-push lands from. Newest run first.
runs_jq='[.[] | select(.headSha == "'"$sha"'" and .event == "push" and (.headBranch == "'"$main_branch"'" or (.headBranch | startswith("train/"))))]'
# An unfinished run has no conclusion; "none" keeps the tab-separated fields
# from collapsing when bash splits the line.
runs_jq+=' | sort_by(-.databaseId) | .[] | [.databaseId, .headBranch, .status, (if (.conclusion // "") == "" then "none" else .conclusion end), .attempt, .url] | @tsv'
if ! runs="$(gh run list -R "$repo" --workflow "$workflow" --commit "$sha" --event push \
  --limit 100 --json databaseId,headSha,headBranch,event,status,conclusion,attempt,url \
  --jq "$runs_jq")"; then
  refuse "could not list $workflow runs for $sha"
fi
# Parallel arrays rather than an associative one: macOS still ships bash 3.2.
green_ids=()
green_descs=()
seen=()
while IFS=$'\t' read -r id branch status conclusion attempt url; do
  [ -n "$id" ] || continue
  seen+=("$url on $branch is $status/$conclusion")
  if [ "$status" = "completed" ] && [ "$conclusion" = "success" ]; then
    green_ids+=("$id")
    green_descs+=("$url (branch $branch, attempt $attempt)")
  fi
done <<< "$runs"
[ "${#seen[@]}" -gt 0 ] ||
  refuse "no $workflow run for $sha was started by a push to $main_branch or train/*; land the commit through scripts/train-push.sh"
if [ "${#green_ids[@]}" -eq 0 ]; then
  summary="$(printf '%s; ' "${seen[@]}")"
  refuse "no green $workflow run for $sha: ${summary%; }"
fi

# --- 3. Every required check of main is a green job of that run -------------
toplevel="$(git rev-parse --show-toplevel)"
required_list="$toplevel/$required_list_rel"
read_required_list() {
  [ -f "$required_list" ] || return 0
  sed -e 's/#.*//' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' "$required_list" | grep -v '^$' || true
}

listed="$(read_required_list)"
if protected="$(gh api "repos/$repo/branches/$main_branch/protection/required_status_checks" \
  --jq '.contexts[]' 2>/dev/null)" && [ -n "$protected" ]; then
  required="$protected"
  source_desc="branch protection on $main_branch"
  if [ -n "$listed" ]; then
    drift="$(diff <(sort -u <<< "$protected") <(sort -u <<< "$listed") || true)"
    if [ -n "$drift" ]; then
      say "warning: $required_list_rel differs from branch protection (< protection, > list); update the list:"
      printf '%s\n' "$drift" | grep '^[<>]' | sed 's/^/release-ci-gate:   /'
    fi
  fi
else
  required="$listed"
  source_desc="$required_list_rel (branch protection is not readable with this token)"
fi
[ -n "$required" ] ||
  refuse "no required checks known: branch protection is unreadable or empty and $required_list_rel is missing or empty"
required_count="$(grep -c . <<< "$required")"
say "required checks: $required_count from $source_desc"

# Try each green run, newest first; one with every required job green is enough.
problems=()
for index in "${!green_ids[@]}"; do
  id="${green_ids[$index]}"
  desc="${green_descs[$index]}"
  # A job without a conclusion is unfinished; report its status instead.
  if ! jobs="$(gh run view "$id" -R "$repo" --json jobs \
    --jq '.jobs[] | [.name, (if (.conclusion // "") == "" then (.status // "unknown") else .conclusion end)] | @tsv')"; then
    problems+=("$desc: could not list its jobs")
    continue
  fi
  bad=()
  while IFS= read -r check; do
    results="$(awk -F'\t' -v name="$check" '$1 == name { print $2 }' <<< "$jobs")"
    if [ -z "$results" ]; then
      bad+=("'$check' missing from the run")
    elif grep -qvx success <<< "$results"; then
      bad+=("'$check' $(paste -sd, - <<< "$results")")
    fi
  done <<< "$required"
  if [ "${#bad[@]}" -eq 0 ]; then
    say "OK: $desc is green with all $required_count required checks success"
    exit 0
  fi
  detail="$(printf '%s; ' "${bad[@]}")"
  problems+=("$desc: ${detail%; }")
done

summary="$(printf '%s | ' "${problems[@]}")"
refuse "no $workflow run for $sha has every required check green: ${summary% | }"
