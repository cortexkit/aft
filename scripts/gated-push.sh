#!/usr/bin/env bash
# Run a verification command and its preflight checks with their original exit
# statuses preserved, then push only on success.
#
# NOT the default push path any more: use scripts/train-push.sh, which pushes a
# train branch and lets CI gate it on all three platforms. This script is for
# the work whose failures only reproduce on this box (watcher/fseventsd, macOS
# exec assessment), where the local full gate is the only gate that can see them.
#
# Usage: scripts/gated-push.sh [--remote origin] [--branch main] -- <gate command...>
# Example: scripts/gated-push.sh -- cargo test -p agent-file-tools --lib
set -euo pipefail

# Run the entire gate at reduced scheduling priority so saturated test
# windows cannot starve the supervised ck-* modules into missing health
# probes (three health-kills on 2026-08-08 traced to gate-window load).
# taskpolicy demotes to the utility QoS class on macOS (E-cores under
# contention); nice covers Linux and the priority dimension everywhere.
# Self-demotion only covers load WE generate; the supervisor-side
# threshold fix covers foreign load.
if [ -z "${AFT_GATE_DEMOTED:-}" ]; then
  export AFT_GATE_DEMOTED=1
  # nice-only, deliberately NOT taskpolicy utility: the QoS class pins the
  # whole gate to E-cores, and the integration suite's per-test 60s response
  # deadlines then fail wholesale (three gate runs red while the same suite
  # was green undemoted). nice yields to the normal-priority ck-* modules
  # under contention but keeps P-cores when the machine has headroom, which
  # is the property the demotion exists for.
  exec nice -n 10 "$0" "$@"
fi

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

# Refuse to gate a tree that is mid-merge/cherry-pick/rebase: a conflicted
# tree can compile stale HEAD state while the gate's pipes mask failures,
# and the final push becomes a no-op "up-to-date" that reads as success.
for marker in CHERRY_PICK_HEAD MERGE_HEAD REBASE_HEAD; do
  if [ -e "$(git rev-parse --git-dir)/$marker" ]; then
    echo "gated-push: refusing — $marker present (unresolved git operation)" >&2
    exit 2
  fi
done
set -o pipefail

remote="origin"
branch="main"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --remote) remote="$2"; shift 2 ;;
    --branch) branch="$2"; shift 2 ;;
    --) shift; break ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [[ $# -eq 0 ]]; then
  echo "no gate command given" >&2
  exit 2
fi

# Keep the complete gate transcript outside /tmp so an OS cleanup cannot erase
# the only diagnosis while a long gate is still running. A caller may provide a
# file with AFT_GATE_LOG or a directory with AFT_GATE_LOG_DIR.
gate_log_dir="${AFT_GATE_LOG_DIR:-$HOME/.local/share/cortexkit/aft/gate-logs}"
gate_log="${AFT_GATE_LOG:-$gate_log_dir/gated-push-$(date -u '+%Y%m%dT%H%M%SZ')-$$.log}"
mkdir -p "$(dirname "$gate_log")"
: > "$gate_log"
export AFT_GATE_LOG="$gate_log"
export AFT_GATE_LOG_INHERITED=1

announce_gate_line() {
  local line="$1"
  printf '%s\n' "$line"
  printf '%s\n' "$line" >> "$gate_log"
}

announce_gate_line "gate log: $gate_log"

report_gate_failure() {
  local label="$1"
  local rc="$2"
  local failed_names

  failed_names="$(grep -F 'FAIL [' "$gate_log" 2>/dev/null || true)"
  {
    printf '==> gate step FAILED: %s (rc=%s)\n' "$label" "$rc"
    printf '==> failing test names (nextest FAIL [ summary)\n'
    if [[ -n "$failed_names" ]]; then
      printf '%s\n' "$failed_names"
    else
      printf '%s\n' '(no nextest FAIL [ summary was emitted)'
    fi
  } >> "$gate_log"

  # Append the failing test names before taking the tail, so fail-fast output still
  # identifies the test that caused the failure even when nextest stops before
  # producing a complete report.
  tail -n 60 "$gate_log" >&2 || true
}

run_logged() {
  local label="$1"
  shift
  local started=$SECONDS
  local -a pipeline_status
  local rc
  local tee_rc

  announce_gate_line "==> $label"
  # tee keeps the command's live output while PIPESTATUS preserves the command
  # status instead of accidentally returning tee's status.
  set +e
  "$@" 2>&1 | tee -a "$gate_log"
  pipeline_status=("${PIPESTATUS[@]}")
  set -e
  rc="${pipeline_status[0]:-1}"
  tee_rc="${pipeline_status[1]:-1}"
  if [[ "$rc" -ne 0 || "$tee_rc" -ne 0 ]]; then
    if [[ "$rc" -eq 0 ]]; then
      rc="$tee_rc"
    fi
    report_gate_failure "$label" "$rc"
    return "$rc"
  fi

  announce_gate_line "    ok ($((SECONDS - started))s)"
}

# Warm the product binary before the verification command spawns it. Cargo's
# linker signature is not enough to avoid a fresh-inode Gatekeeper assessment;
# sign with a stable identifier and execute once to pay that assessment outside
# timed integration tests. Do not re-sign an already-stably-signed inode because
# replacing it would throw away its warm result.
warm_macos_product_binary() {
  [[ "$(uname)" == "Darwin" ]] || return 0
  local target_dir="${CARGO_TARGET_DIR:-target}"
  local aft_binary="$target_dir/debug/aft"
  [[ -x "$aft_binary" ]] || return 0

  if ! codesign -d -vv "$aft_binary" 2>&1 | grep -Fq 'Identifier=aft-dev-gate'; then
    codesign -f -s - --identifier aft-dev-gate "$aft_binary" >/dev/null 2>&1 || true
  fi
  "$aft_binary" --version >/dev/null 2>&1 || true
}

warm_macos_product_binary
run_logged "gated-push: running gate: $*" "$@"

run_logged "gated-push: governed-surface audit" bun scripts/audit-v049-agent-surface.ts
run_logged "gated-push: release gate" node scripts/release-gate-v049.mjs

# Keep main fmt-clean per push. Put the helpful remediation in the logged step
# while preserving a nonzero status for drift.
cargo_fmt_check() {
  cargo fmt --all -- --check || {
    echo "gated-push: cargo fmt drift — run 'cargo fmt --all'" >&2
    return 1
  }
}
run_logged "gated-push: cargo fmt check" cargo_fmt_check

# Biome at the workspace root follows Bun's workspace graph and checks every
# package, including aft-cli and benchmarks, rather than only plugin/src trees.
run_logged "gated-push: workspace Biome check" bunx biome check .

announce_gate_line "gated-push: preflight green — pushing to $remote $branch"
run_logged "gated-push: git push $remote $branch" git push "$remote" "$branch"

# Outcome check, not just command check: a push can report success through a
# wrapper (or fail on auth) while origin never moved. Non-empty @{u}..HEAD
# after a "successful" push is the tell that survives any false green.
git fetch -q "$remote" "$branch"
remote_ref="${remote}/${branch}"
unpushed=$(git rev-list --count "${remote_ref}..${branch}")
if [[ "$unpushed" -ne 0 ]]; then
  announce_gate_line "gated-push: push reported success but ${unpushed} commit(s) not on ${remote}/${branch} — origin did not move" >&2
  exit 1
fi
announce_gate_line "gated-push: push verified on ${remote}/${branch}"
