#!/usr/bin/env bash
# Tests for card-source-gate.sh against a throwaway repository and fake cards.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$SCRIPT_DIR/card-source-gate.sh"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/card-source-gate-test.XXXXXX")"
trap 'rm -rf "$TMP_DIR"' EXIT

REPO="$TMP_DIR/repo"
STAGING="$TMP_DIR/staging"
DEPLOY="$TMP_DIR/bin/ck-aft"
mkdir -p "$REPO" "$STAGING" "$TMP_DIR/bin"
cd "$REPO"
git init -q -b main
git config user.email test@example.com
git config user.name test
commit() { echo "$1" > "$1.txt"; git add "$1.txt"; git commit -q -m "$1"; git rev-parse HEAD; }

BASE="$(commit base)"
FIX="$(commit fix)"
git checkout -q -b other "$BASE"
OTHER="$(commit other)"

# A fake card embeds its source commit the way AFT_BUILD_GIT_SHA does, with
# hex digits on both sides as when string literals sit back to back.
card_from() { printf 'binary\0deadbeef%sc0ffee\0more bytes\0' "$1" > "$DEPLOY"; }

run_gate() { (cd "$REPO" && source "$GATE" && ck_aft_check_card_source "$DEPLOY" "$STAGING" test) >"$TMP_DIR/out" 2>&1; }
expect_pass() { run_gate || { echo "FAIL ($1): expected pass" >&2; cat "$TMP_DIR/out" >&2; exit 1; }; }
expect_refuse() {
  if run_gate; then echo "FAIL ($1): expected refusal" >&2; cat "$TMP_DIR/out" >&2; exit 1; fi
  grep -q "$2" "$TMP_DIR/out" || { echo "FAIL ($1): output lacks '$2'" >&2; cat "$TMP_DIR/out" >&2; exit 1; }
}

# The running card was built from main's fix; HEAD on `other` lacks it.
card_from "$FIX"
git checkout -q other
expect_refuse "branch without the running card's fix" "${FIX:0:12}"

# Cherry-picking the fix brings its change in, so the card passes.
git cherry-pick "$FIX" >/dev/null
expect_pass "fix cherry-picked"

# Leaving the fix out on purpose passes only when acknowledged.
git reset -q --hard "$OTHER"
STAGE_ACK_MISSING="${FIX:0:9}" expect_pass "fix acknowledged by prefix"
STAGE_ACK_MISSING="${FIX:0:6}" expect_refuse "too-short acknowledgement" "${FIX:0:12}"
STAGE_ACK_MISSING=all expect_pass "all acknowledged"

# A card built on top of the running card's source passes.
git checkout -q main
commit next >/dev/null
expect_pass "descendant of the running card's source"

# A sidecar beside the staged card takes precedence over the embedded id.
hash="$(shasum -a 256 "$DEPLOY" | awk '{print $1}')"
echo "$OTHER" > "$STAGING/ck-aft.${hash:0:16}.source"
expect_refuse "sidecar names a source main lacks" "${OTHER:0:12}"
rm "$STAGING/ck-aft.${hash:0:16}.source"

# When the running card's source can't be found, refuse unless acknowledged.
printf 'binary without a commit id\0' > "$DEPLOY"
expect_refuse "unknown running source" "can't tell which commit"
STAGE_ACK_MISSING=all expect_pass "unknown source acknowledged"

# No running card: nothing to compare.
rm "$DEPLOY"
expect_pass "no running card"

echo "card-source-gate: all tests passed"
