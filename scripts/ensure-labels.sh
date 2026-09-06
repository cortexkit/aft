#!/usr/bin/env bash
# Operator tooling runs through the real GitHub CLI (gh); the shim is only for AI agent commands.
# Create (or realign) the two labels the design gate reads:
#
#   design-approved  on an ISSUE: a maintainer agreed the design, so a pull
#                    request closing it may be reviewed and merged.
#   trivial          on a PULL REQUEST: typo-class change, gate does not apply.
#                    Applied by a maintainer, so the bypass stays in the audit
#                    trail.
#
# Run by hand, once per repository. Nothing in CI or in the test suite creates
# labels — this touches the real repository.
#
# Usage:
#   scripts/ensure-labels.sh                # cortexkit/aft
#   scripts/ensure-labels.sh owner/repo     # a fork or another repository
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/operator-gh.sh" || exit 1

REPO="${1:-cortexkit/aft}"

# Idempotent: create the label when it is missing, otherwise align its colour
# and description, so re-running after an edit converges instead of failing.
ensure_label() {
  local name="$1" color="$2" description="$3"

  if "$OPERATOR_GH" label list --repo "$REPO" --search "$name" --json name --jq '.[].name' |
    grep -qxF -- "$name"; then
    "$OPERATOR_GH" label edit "$name" --repo "$REPO" --color "$color" --description "$description"
    echo "ensure-labels: $REPO $name (updated)"
  else
    "$OPERATOR_GH" label create "$name" --repo "$REPO" --color "$color" --description "$description"
    echo "ensure-labels: $REPO $name (created)"
  fi
}

ensure_label "design-approved" "0E8A16" \
  "Design agreed by a maintainer; a PR closing this issue can be reviewed"
ensure_label "trivial" "C2E0C6" \
  "Typo-class change; exempt from the design-approved gate"
