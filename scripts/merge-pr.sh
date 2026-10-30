#!/usr/bin/env bash
# Operator tooling runs through the real GitHub CLI (gh); the shim is only for AI agent commands.
# Gated PR merge - the ONLY sanctioned way to merge a contributor PR.
#
# Exists because of #234 (2026-08-17): a P1 bot finding landed at 12:10, the
# author pushed the fix at 12:17, and a hand merge at ~12:19 took the head
# WITHOUT the fix - forcing a post-merge follow-up PR. A merge must not race
# an iterating author or an open finding.
#
# Refuses to merge when:
#   1. any review thread is unresolved,
#   2. any review/review-comment is NEWER than the head commit (a finding the
#      author has not pushed over yet),
#   3. the head was pushed very recently (author likely mid-iteration),
#   4. CI on the head is red/pending (merge rides gh's own check gate).
# and, after a merge-queue submission, refuses (with auto-merge disarmed) when
# GitHub does not list the PR in the queue within a bounded wait: gh can report
# a submission as accepted while no queue entry is ever created.
# Gates 1-4 are read again immediately before every submission, including a
# resubmission, so a review posted during a wait cannot ride an earlier result.
#
# Usage: scripts/merge-pr.sh <pr-number> [--settle-minutes N] [--force-reason "..."]
#        [--expected-head SHA] [--legacy-merge]
set -euo pipefail

PR="${1:?usage: merge-pr.sh <pr-number> [--settle-minutes N] [--force-reason ...]}"
shift || true
SETTLE_MIN=5
FORCE_REASON=""
LEGACY_MERGE=0
EXPECTED_HEAD=""
while [ $# -gt 0 ]; do
  case "$1" in
    --settle-minutes) SETTLE_MIN="$2"; shift 2 ;;
    --force-reason) FORCE_REASON="$2"; shift 2 ;;
    --legacy-merge) LEGACY_MERGE=1; shift ;;
    --expected-head) EXPECTED_HEAD="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib/operator-gh.sh" || exit 1

OWNER_REPO=$("$OPERATOR_GH" repo view --json nameWithOwner --jq .nameWithOwner)
OWNER="${OWNER_REPO%%/*}"
REPO="${OWNER_REPO##*/}"

refuse() {
  echo "merge-pr: REFUSED - $1" >&2
  if [ -n "$FORCE_REASON" ]; then
    echo "merge-pr: proceeding anyway (--force-reason: $FORCE_REASON)" >&2
    return 0
  fi
  exit 1
}

hard_refuse() {
  echo "merge-pr: REFUSED - $1" >&2
  exit 1
}

# $1 says when the disarm happens, for the log lines; it defaults to the
# pre-submission case.
disarm_auto_merge() {
  local when="${1:-before merge admission}" state pull_request_id queued auto_merge
  state=$("$OPERATOR_GH" api graphql -f query='
    query($owner:String!,$repo:String!,$pr:Int!){
      repository(owner:$owner,name:$repo){
        pullRequest(number:$pr){id isInMergeQueue autoMergeRequest{enabledAt}}
      }
    }' -F owner="$OWNER" -F repo="$REPO" -F pr="$PR" \
    --jq '[.data.repository.pullRequest.id, .data.repository.pullRequest.isInMergeQueue, (.data.repository.pullRequest.autoMergeRequest != null)] | @tsv') ||
    hard_refuse "could not inspect auto-merge/queue state $when"
  IFS=$'\t' read -r pull_request_id queued auto_merge <<< "$state"
  [[ -n "$pull_request_id" ]] || hard_refuse "could not identify PR #$PR to disarm auto-merge"
  if [[ "$queued" == true ]]; then
    "$OPERATOR_GH" api graphql -f query='mutation($id:ID!){dequeuePullRequest(input:{id:$id}){clientMutationId}}' \
      -F id="$pull_request_id" >/dev/null || hard_refuse "could not dequeue PR #$PR $when"
    echo "merge-pr: dequeued #$PR $when" >&2
  fi
  if [[ "$auto_merge" == true ]]; then
    "$OPERATOR_GH" api graphql -f query='mutation($id:ID!){disablePullRequestAutoMerge(input:{pullRequestId:$id}){clientMutationId}}' \
      -F id="$pull_request_id" >/dev/null || hard_refuse "could not disable auto-merge for PR #$PR $when"
    echo "merge-pr: disabled auto-merge for #$PR $when" >&2
  fi
}

wait_for_greptile_review() {
  local head_sha="$1" timeout poll absent_grace started now review_status review_conclusion review_result
  timeout="${AFT_MERGE_PR_GREPTILE_TIMEOUT_SECONDS:-1200}"
  poll="${AFT_MERGE_PR_GREPTILE_POLL_SECONDS:-15}"
  # Greptile creates its check run (queued) within seconds of a push. When it
  # has created nothing for a head after this long, it is not reviewing this
  # repository (an outage or a spent allowance), and the PR goes ahead without
  # it. A review that exists is still waited for, and a failed one still
  # refuses. Set it to 0 to require a Greptile review again.
  absent_grace="${AFT_MERGE_PR_GREPTILE_ABSENT_GRACE_SECONDS:-180}"
  started=$(date -u +%s)
  while :; do
    review_result=$("$OPERATOR_GH" api "repos/$OWNER_REPO/commits/$head_sha/check-runs?per_page=100" \
      --jq '[.check_runs[] | select(.head_sha == "'"$head_sha"'" and .name == "Greptile Review" and .app.slug == "greptile-apps")] | sort_by(.started_at // .created_at // "") | if length == 0 then ["", ""] else [.[-1].status, (.[-1].conclusion // "")] end | @tsv') ||
      hard_refuse "could not read Greptile Review status for head $head_sha"
    IFS=$'\t' read -r review_status review_conclusion <<< "$review_result"
    if [[ "$review_status" == completed ]]; then
      case "$review_conclusion" in
        success|neutral)
          echo "merge-pr: Greptile Review concluded $review_conclusion for head $head_sha" >&2
          return 0
          ;;
        *)
          hard_refuse "Greptile Review concluded ${review_conclusion:-<missing>} for head $head_sha; refusing submission"
          ;;
      esac
    fi
    now=$(date -u +%s)
    if [[ -z "$review_status" ]] && (( absent_grace > 0 && now - started >= absent_grace )); then
      echo "merge-pr: WARNING: Greptile created no review for head $head_sha within ${absent_grace}s; submitting without it" >&2
      return 0
    fi
    if (( now - started >= timeout )); then
      hard_refuse "Greptile Review did not complete for head $head_sha within ${timeout}s; refusing blind submission"
    fi
    sleep "$poll"
  done
}

# One GraphQL read of the PR's lifecycle state, GitHub's merge-conflict
# computation, queue membership and auto-merge arming, as four tab-separated
# fields: state, mergeable (MERGEABLE/CONFLICTING/UNKNOWN), isInMergeQueue,
# and whether an auto-merge request is armed.
read_queue_state() {
  "$OPERATOR_GH" api graphql -f query='
    query($owner:String!,$repo:String!,$pr:Int!){
      repository(owner:$owner,name:$repo){
        pullRequest(number:$pr){state mergeable isInMergeQueue autoMergeRequest{enabledAt}}
      }
    }' -F owner="$OWNER" -F repo="$REPO" -F pr="$PR" \
    --jq '[.data.repository.pullRequest.state, .data.repository.pullRequest.mergeable, .data.repository.pullRequest.isInMergeQueue, (.data.repository.pullRequest.autoMergeRequest != null)] | @tsv'
}

load_queue_state() {
  local state
  state=$(read_queue_state) || return 1
  PR_STATE=""
  PR_MERGEABLE=""
  PR_QUEUED=""
  PR_ARMED=""
  IFS=$'\t' read -r PR_STATE PR_MERGEABLE PR_QUEUED PR_ARMED <<< "$state"
  [[ -n "$PR_STATE" && -n "$PR_MERGEABLE" ]]
}

# GitHub computes mergeability lazily and reports UNKNOWN until it has. A queue
# submission made while it is UNKNOWN can be accepted without the PR ever
# entering the queue, so wait (bounded) for a computed value first. A reviewer
# can open a thread or post a failing review during the wait without moving the
# head, so every sleep is followed by a fresh run of the admission gates: the
# submission that follows always rides on gate results read after the last wait.
wait_for_mergeable() {
  local timeout poll started now
  timeout="${AFT_MERGE_PR_MERGEABLE_TIMEOUT_SECONDS:-60}"
  poll="${AFT_MERGE_PR_MERGEABLE_POLL_SECONDS:-5}"
  started=$(date -u +%s)
  while :; do
    load_queue_state || hard_refuse "could not read mergeability of PR #$PR"
    [[ "$PR_MERGEABLE" != UNKNOWN ]] && return 0
    now=$(date -u +%s)
    if (( now - started >= timeout )); then
      hard_refuse "GitHub has not computed mergeability for PR #$PR within ${timeout}s (mergeable UNKNOWN); refusing blind queue submission, rerun later"
    fi
    echo "merge-pr: GitHub reports mergeable UNKNOWN for #$PR; waiting for it to be computed" >&2
    sleep "$poll"
    run_admission_gates
  done
}

# Submit with the operator's real GitHub CLI, not the agent's bot-routed shim.
# The response is kept so a later refusal can quote what GitHub said.
submit_merge() {
  local status=0
  MERGE_RESPONSE=$(GH_SHIM_BYPASS=operator "$OPERATOR_GH" pr merge "$PR" "${MERGE_ARGS[@]}" 2>&1) || status=$?
  if [ -n "$MERGE_RESPONSE" ]; then
    printf '%s\n' "$MERGE_RESPONSE" >&2
  fi
  if [ "$status" -ne 0 ]; then
    echo "merge-pr: gh pr merge failed for #$PR (exit $status)" >&2
    exit "$status"
  fi
}

# Reads every required status context configured for the PR's base branch,
# then overlays the results GitHub has reported for this head. A configured
# context without a result is still a pending check, not an absent requirement.
read_required_checks() {
  local protection_output rules_output output protection_error rules_error
  local exit_code=0 name bucket context existing found i name_index classic_not_found=false
  local required_count=0 reported_count=0
  local -a required_contexts=() reported_names=()
  REQUIRED_PENDING=()
  REQUIRED_PROBLEM=""

  if protection_output=$("$OPERATOR_GH" api "repos/$OWNER_REPO/branches/$BASE_BRANCH/protection/required_status_checks" \
      --jq 'if type != "object" then error("expected required-status-checks object") elif .checks? != null and (.checks | type) != "array" then error("malformed required checks") elif .contexts? != null and (.contexts | type) != "array" then error("malformed required contexts") elif ((.checks // []) | length) > 0 then .checks[] | if (.context | type) == "string" and .context != "" then .context else error("malformed required-check context") end elif ((.contexts // []) | length) > 0 then .contexts[] | if type == "string" and . != "" then . else error("malformed required context") end else empty end' 2>&1); then
    :
  else
    protection_error=$protection_output
    if [[ "$protection_error" =~ HTTP[[:space:]]+404|404[[:space:]]+Not[[:space:]]+Found ]]; then
      # GitHub returns 404 when classic branch protection is not configured.
      # The effective-rules endpoint below still determines whether checks apply.
      classic_not_found=true
      protection_output=""
    elif [[ "$protection_error" == *malformed* || "$protection_error" == *"expected required-status-checks object"* ]]; then
      REQUIRED_PROBLEM="malformed branch-protection required checks for base branch '$BASE_BRANCH' on PR #$PR"
      return 0
    else
      REQUIRED_PROBLEM="could not read branch-protection required checks for base branch '$BASE_BRANCH' on PR #$PR"
      return 0
    fi
  fi

  # This endpoint returns the active rules that apply to the named branch,
  # including repository rulesets and inherited rulesets.
  if rules_output=$("$OPERATOR_GH" api "repos/$OWNER_REPO/rules/branches/$BASE_BRANCH" \
      --jq 'if type != "array" then error("expected effective branch-rules list") else .[] | select(.type == "required_status_checks") | if (.parameters.required_status_checks | type) != "array" then error("malformed required-status-check rule") else .parameters.required_status_checks[] | if (.context | type) == "string" and .context != "" then .context else error("malformed ruleset context") end end end' 2>&1); then
    :
  else
    rules_error=$rules_output
    if [[ "$rules_error" == *malformed* || "$rules_error" == *"expected effective branch-rules list"* ]]; then
      REQUIRED_PROBLEM="malformed effective ruleset checks for base branch '$BASE_BRANCH' on PR #$PR"
    elif [[ "$classic_not_found" == true ]]; then
      REQUIRED_PROBLEM="could not read effective ruleset checks for base branch '$BASE_BRANCH' on PR #$PR (classic branch protection returned 404)"
    else
      REQUIRED_PROBLEM="could not read effective ruleset checks for base branch '$BASE_BRANCH' on PR #$PR"
    fi
    return 0
  fi

  while IFS= read -r context; do
    [ -n "$context" ] || continue
    found=false
    for ((i = 0; i < required_count; i++)); do
      existing=${required_contexts[i]}
      if [[ "$existing" == "$context" ]]; then
        found=true
        break
      fi
    done
    if [[ "$found" != true ]]; then
      required_contexts+=("$context")
      required_count=$((required_count + 1))
    fi
  done <<< "$(printf '%s\n%s\n' "$protection_output" "$rules_output")"

  output=$("$OPERATOR_GH" pr checks "$PR" --required --json name,bucket \
      --jq '.[] | [.name, .bucket] | @tsv') || exit_code=$?
  # gh exits 1 when a check failed and 8 when one is pending; anything else
  # means the checks could not be read at all.
  if [ "$exit_code" -ne 0 ] && [ "$exit_code" -ne 1 ] && [ "$exit_code" -ne 8 ]; then
    REQUIRED_PROBLEM="could not read required checks for PR #$PR"
    return 0
  fi
  if [ -n "$output" ]; then
    while IFS=$'\t' read -r name bucket; do
      if [ -z "$name" ] || [ -z "$bucket" ]; then
        REQUIRED_PROBLEM="malformed required-check result for PR #$PR"
        return 0
      fi
      reported_names+=("$name")
      reported_count=$((reported_count + 1))
      case "$bucket" in
        pass|skipping) ;;
        pending) REQUIRED_PENDING+=("$name") ;;
        fail|cancel)
          REQUIRED_PROBLEM="required check '$name' did not pass ($bucket)"
          return 0
          ;;
        *)
          REQUIRED_PROBLEM="required check '$name' has unknown status '$bucket'"
          return 0
          ;;
      esac
    done <<< "$output"
  fi

  for ((i = 0; i < required_count; i++)); do
    context=${required_contexts[i]}
    found=false
    for ((name_index = 0; name_index < reported_count; name_index++)); do
      name=${reported_names[name_index]}
      if [[ "$name" == "$context" ]]; then
        found=true
        break
      fi
    done
    [[ "$found" == true ]] || REQUIRED_PENDING+=("$context")
  done

  if [ "$required_count" -eq 0 ] && [ -z "$output" ]; then
    REQUIRED_PROBLEM="no required checks found for PR #$PR"
    return 0
  fi
  if [ "$exit_code" -eq 1 ]; then
    REQUIRED_PROBLEM="gh pr checks failed without a named required-check result for PR #$PR"
  elif [ "$exit_code" -eq 8 ] && [ "${#REQUIRED_PENDING[@]}" -eq 0 ]; then
    REQUIRED_PROBLEM="gh reported pending required checks without naming a pending check for PR #$PR"
  fi
}

# gh can report a queue submission as accepted while GitHub never creates the
# queue entry. Poll (bounded) until GitHub lists the PR in the queue or merged.
# While required checks are still running, GitHub only arms auto-merge and adds
# the PR once they pass, so an armed request also counts, but only while a fresh
# read shows a required check still running and none red. Checks that finished
# during the waits since admission leave nothing for GitHub to wait on, so an
# armed request without a queue entry then keeps the poll going to its bound.
# Returns 1 when the wait ends without confirmation; PR_* hold the last
# observation.
confirm_queue_entry() {
  local timeout poll started now
  timeout="${AFT_MERGE_PR_QUEUE_CONFIRM_TIMEOUT_SECONDS:-60}"
  poll="${AFT_MERGE_PR_QUEUE_CONFIRM_POLL_SECONDS:-5}"
  started=$(date -u +%s)
  while :; do
    if ! load_queue_state; then
      disarm_auto_merge "after an unconfirmed queue submission"
      hard_refuse "submitted PR #$PR but could not read whether it entered the merge queue; auto-merge disarmed"
    fi
    [[ "$PR_STATE" == MERGED || "$PR_QUEUED" == true ]] && return 0
    if [[ "$PR_ARMED" == true ]]; then
      read_required_checks
      [[ -z "$REQUIRED_PROBLEM" && "${#REQUIRED_PENDING[@]}" -gt 0 ]] && return 0
    fi
    now=$(date -u +%s)
    (( now - started >= timeout )) && return 1
    sleep "$poll"
  done
}

# The admission gates, run before every submission. Each run re-reads the
# reviews of the pinned head, the review threads and findings, the settle
# window, the queue policy and the required checks, and exits on a refusal.
run_admission_gates() {
  # --- 1. Wait for the automated review of the exact pinned head -----------
  wait_for_greptile_review "$HEAD_SHA"
  CURRENT_HEAD=$("$OPERATOR_GH" pr view "$PR" --json headRefOid --jq .headRefOid)
  if [[ "$CURRENT_HEAD" != "$HEAD_SHA" ]]; then
    echo "merge-pr: REFUSED - PR head changed since $HEAD_SHA was pinned for admission; not overridable" >&2
    exit 1
  fi

  # --- 2. Unresolved review threads ------------------------------------------
  UNRESOLVED=0
  THREAD_CURSOR=""
  while :; do
    THREAD_QUERY='query($owner:String!,$repo:String!,$pr:Int!,$after:String){
      repository(owner:$owner,name:$repo){
        pullRequest(number:$pr){
          reviewThreads(first:100,after:$after){nodes{isResolved} pageInfo{hasNextPage endCursor}}
        }
      }
    }'
    THREAD_ARGS=(api graphql -f "query=$THREAD_QUERY" -F owner="$OWNER" -F repo="$REPO" -F pr="$PR")
    [[ -n "$THREAD_CURSOR" ]] && THREAD_ARGS+=(-F after="$THREAD_CURSOR")
    THREAD_PAGE=$("$OPERATOR_GH" "${THREAD_ARGS[@]}" \
      --jq '[([.data.repository.pullRequest.reviewThreads.nodes[]? | select(.isResolved==false)] | length), (.data.repository.pullRequest.reviewThreads.pageInfo.hasNextPage // false), (.data.repository.pullRequest.reviewThreads.pageInfo.endCursor // "")] | @tsv') ||
      hard_refuse "could not read review threads for PR #$PR"
    THREAD_COUNT=""
    THREAD_HAS_NEXT=""
    THREAD_NEXT_CURSOR=""
    IFS=$'\t' read -r THREAD_COUNT THREAD_HAS_NEXT THREAD_NEXT_CURSOR <<< "$THREAD_PAGE"
    [[ "$THREAD_COUNT" =~ ^[0-9]+$ ]] || hard_refuse "malformed review-thread page for PR #$PR"
    UNRESOLVED=$((UNRESOLVED + THREAD_COUNT))
    [[ "$THREAD_HAS_NEXT" == true ]] || break
    [[ -n "$THREAD_NEXT_CURSOR" ]] || hard_refuse "review-thread pagination returned no cursor for PR #$PR"
    THREAD_CURSOR="$THREAD_NEXT_CURSOR"
  done
  if [ "$UNRESOLVED" -ne 0 ]; then
    disarm_auto_merge
    refuse "$UNRESOLVED unresolved review thread(s); resolve or let the author finish"
  fi

  # --- 3. Findings newer than the head commit --------------------------------
  HEAD_TS=$("$OPERATOR_GH" api "repos/$OWNER_REPO/commits/$HEAD_SHA" --jq .commit.committer.date)
  # Fetch comments and reviews separately so a failed comments query cannot be
  # hidden by the successful reviews query or the counting pipeline.
  COMMENTS=$("$OPERATOR_GH" api "repos/$OWNER_REPO/pulls/$PR/comments" --paginate --jq '.[].created_at')
  REVIEWS=$("$OPERATOR_GH" api "repos/$OWNER_REPO/pulls/$PR/reviews" --paginate --jq '.[] | select(.state != "APPROVED" and .state != "PENDING") | .submitted_at')
  NEWER=$(printf '%s\n%s\n' "$COMMENTS" "$REVIEWS" | awk -v head="$HEAD_TS" '$0 > head' | wc -l | tr -d ' ')
  if [ "$NEWER" -ne 0 ]; then
    # The check exists to stop merging while an author is still addressing
    # findings. When the maintainer resolved the outstanding threads with a
    # recorded rationale (so newer comments are the resolution itself, not
    # unaddressed findings), the maintainer can acknowledge them explicitly.
    # The reason is printed so the merge log carries the judgment.
    if [ -n "${AFT_MERGE_PR_ACK_FINDINGS:-}" ]; then
      echo "merge-pr: ACK - $NEWER post-head comment(s) acknowledged by maintainer: ${AFT_MERGE_PR_ACK_FINDINGS}"
    else
      refuse "$NEWER review comment(s)/review(s) are newer than the head commit - the author has not pushed over the latest finding (set AFT_MERGE_PR_ACK_FINDINGS=\"reason\" to acknowledge deliberately)"
    fi
  fi

  # --- 4. Author mid-iteration (head pushed too recently) --------------------
  NOW_EPOCH=$(date -u +%s)
  HEAD_EPOCH=$(python3 -c "import datetime;print(int(datetime.datetime.fromisoformat('$HEAD_TS'.replace('Z','+00:00')).timestamp()))")
  AGE_MIN=$(( (NOW_EPOCH - HEAD_EPOCH) / 60 ))
  [ "$AGE_MIN" -ge "$SETTLE_MIN" ] || refuse "head commit is ${AGE_MIN}m old (< ${SETTLE_MIN}m settle window) - author may be mid-iteration"

  # --- 5. Queue admission (upstream gh enforces required checks) -------------
  # REST's branch-protection response omits merge-queue policy. The PR's
  # isMergeQueueEnabled GraphQL field is the effective base-ref policy used by
  # the real GitHub CLI; an absent/error response never permits a legacy merge.
  POLICY=$("$OPERATOR_GH" api graphql -f query='
    query($owner:String!,$repo:String!,$pr:Int!){
      repository(owner:$owner,name:$repo){
        pullRequest(number:$pr){baseRefName isMergeQueueEnabled}
      }
    }' -F owner="$OWNER" -F repo="$REPO" -F pr="$PR" \
    --jq '[.data.repository.pullRequest.baseRefName, .data.repository.pullRequest.isMergeQueueEnabled] | @tsv')
  IFS=$'\t' read -r BASE_BRANCH QUEUE_ENABLED <<< "$POLICY"
  if [[ "$BASE_BRANCH" != main || ("$QUEUE_ENABLED" != true && "$QUEUE_ENABLED" != false) ]]; then
    echo "merge-pr: REFUSED - unknown queue policy or non-main base; not overridable" >&2
    exit 1
  fi
  MERGE_ARGS=(--match-head-commit "$HEAD_SHA")
  if [[ "$QUEUE_ENABLED" == true ]]; then
    if [[ "$LEGACY_MERGE" == 1 ]]; then
      echo "merge-pr: REFUSED - --legacy-merge cannot bypass enabled queue protection" >&2
      exit 1
    fi
  elif [[ "$LEGACY_MERGE" == 1 ]]; then
    MERGE_ARGS+=(--merge)
  else
    echo "merge-pr: REFUSED - queue is not enabled; explicit --legacy-merge is required before cutover" >&2
    exit 1
  fi

  AUTO_MERGE_ALLOWED=false
  if [[ "$QUEUE_ENABLED" == true ]]; then
    AUTO_MERGE_ALLOWED=$("$OPERATOR_GH" api "repos/$OWNER_REPO" --jq .allow_auto_merge) ||
      hard_refuse "could not read the repository's auto-merge setting"
  fi

  # With auto-merge allowed, GitHub admits the PR to the queue itself once its
  # required checks pass, so one read is enough: refuse a red check by name, and
  # otherwise submit. Without auto-merge, a merge request on a PR whose checks are
  # still running is refused by GitHub, so refuse here too and let the caller
  # rerun once they pass. Never wait for checks inside this script: the review
  # gates above would go stale. The only waits are short and bounded: for GitHub
  # to compute mergeability before submitting (each followed by a fresh gate run),
  # and for the queue entry to appear after submitting (the head stays pinned by
  # --match-head-commit throughout).
  if [[ "$QUEUE_ENABLED" == true ]]; then
    read_required_checks
    [ -z "$REQUIRED_PROBLEM" ] || hard_refuse "$REQUIRED_PROBLEM"
    if [ "${#REQUIRED_PENDING[@]}" -gt 0 ]; then
      if [[ "$AUTO_MERGE_ALLOWED" == true ]]; then
        echo "merge-pr: required checks still running for #$PR; GitHub queues it when they pass" >&2
      else
        hard_refuse "required checks still running on PR #$PR and auto-merge is off; rerun once they pass: ${REQUIRED_PENDING[*]}"
      fi
    fi
  fi
}

# Every submission, the first and the resubmission alike, goes through here so
# the gates it rides on can never drift from the initial admission: the gates
# run, the mergeability wait re-runs them after each sleep, and the submission
# follows the last gate run with no wait in between.
admit_and_submit() {
  run_admission_gates
  [[ "$QUEUE_ENABLED" == true ]] && wait_for_mergeable
  echo "merge-pr: admission gates satisfied (head ${AGE_MIN}m settled) - submitting #$PR at $HEAD_SHA"
  submit_merge
}

# Clear any previous queue admission before waiting: GitHub can merge an armed
# PR while a review gate is still running, and refusals must leave it disarmed.
disarm_auto_merge

# The head is pinned once; every gate run below must still see this exact head.
HEAD_SHA=$("$OPERATOR_GH" pr view "$PR" --json headRefOid --jq .headRefOid)
if [[ -n "$EXPECTED_HEAD" && "$HEAD_SHA" != "$EXPECTED_HEAD" ]]; then
  echo "merge-pr: REFUSED - PR head changed from the accepted delivery; not overridable" >&2
  exit 1
fi

admit_and_submit
[[ "$QUEUE_ENABLED" == true ]] || exit 0
CONFIRM_TIMEOUT="${AFT_MERGE_PR_QUEUE_CONFIRM_TIMEOUT_SECONDS:-60}"
if confirm_queue_entry; then
  echo "merge-pr: confirmed #$PR with GitHub (state $PR_STATE, in merge queue $PR_QUEUED, auto-merge armed $PR_ARMED)" >&2
  exit 0
fi

REPORTED="state $PR_STATE, mergeable $PR_MERGEABLE, isInMergeQueue $PR_QUEUED, auto-merge armed $PR_ARMED"
RESPONSES="enqueue response: '${MERGE_RESPONSE:-<empty>}'"
# Only an uncomputed mergeability explains a silently dropped submission well
# enough to resubmit; any other state is refused as-is for the operator.
if [[ "$PR_MERGEABLE" == UNKNOWN ]]; then
  echo "merge-pr: #$PR is not in the merge queue ${CONFIRM_TIMEOUT}s after submission ($REPORTED); resubmitting once" >&2
  disarm_auto_merge "before resubmitting to the merge queue"
  admit_and_submit
  if confirm_queue_entry; then
    echo "merge-pr: confirmed #$PR with GitHub after resubmission (state $PR_STATE, in merge queue $PR_QUEUED, auto-merge armed $PR_ARMED)" >&2
    exit 0
  fi
  REPORTED="first submission: $REPORTED; resubmission: state $PR_STATE, mergeable $PR_MERGEABLE, isInMergeQueue $PR_QUEUED, auto-merge armed $PR_ARMED"
  RESPONSES="$RESPONSES; resubmission response: '${MERGE_RESPONSE:-<empty>}'"
fi
disarm_auto_merge "after an unconfirmed queue submission"
hard_refuse "PR #$PR was submitted but GitHub never listed it in the merge queue within ${CONFIRM_TIMEOUT}s ($REPORTED; $RESPONSES); auto-merge disarmed - inspect the PR on GitHub and rerun"
