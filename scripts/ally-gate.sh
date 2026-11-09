#!/usr/bin/env bash
# Native-Windows pre-push gate on the ROG Ally box.
#
# Runs the given nextest filter (default: the Windows-sensitive integration
# families) against HEAD on real Windows hardware with warm incremental
# builds. Calibrated 2026-07-13: warm no-change run 4s end-to-end; warm
# incremental after a small diff, minutes. A cold CI roundtrip is ~35 min.
# CI on GitHub remains the public arbiter — this exists so it stops being the
# debugger.
#
# Usage:
#   scripts/ally-gate.sh                     # default Windows-sensitive set
#   scripts/ally-gate.sh 'test(subc_storm)'  # explicit nextest filter
#   ALLY=user@host scripts/ally-gate.sh      # override target machine
set -euo pipefail

# The Ally takes DHCP leases; if this address goes stale, ping-sweep the /24
# and probe ssh as ufuka (hostname reports AsusAllyKO).
# mDNS first: the DHCP address drifts across re-images (.42 -> .33 so far).
# Override with ALLY=user@host. Box conventions (SYNAPSE-shared): mkdir lock
# at C:\Users\ufuka\ally.lock with HOLDER.txt; ssh DefaultShell is git-bash, so remote paths use ~ (POSIX), not %USERPROFILE%.
ALLY="${ALLY:-ufuka@asusallyko.local}"
FILTER="${1:-test(status_memory) | test(refresh_worker_tests) | test(subc_storm) | test(subc_bridge) | test(bash_background)}"

# Direct git-over-ssh to Windows sshd is broken by cmd.exe quoting (the
# remote helper's quoted repo path reaches cmd as ''aft''), so ship refs via
# the origin ally-gate branch instead: push there, have the Ally fetch it.
# Incremental object transfer keeps both hops in the seconds range.
echo "== pushing HEAD to origin/ally-gate"
git push origin "+HEAD:refs/heads/ally-gate"

echo "== nextest on ally: $FILTER"
# Bare runner over ssh; ssh propagates the remote exit code, which is this
# script's verdict. No pipes between the runner and the gate (rule 8231).
ssh -o BatchMode=yes "$ALLY" \
  "cd ~/aft && git fetch origin ally-gate && git reset --hard FETCH_HEAD && cargo nextest run --features test-hooks -p agent-file-tools -E \"$FILTER\" --no-fail-fast"
rc=$?
echo "ALLY_GATE_RC=$rc"
exit $rc
