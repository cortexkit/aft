#!/usr/bin/env bash
# Cut a daemon placement card for SUBC from the current checkout.
#
# A "card" is the release binary staged for the daemon supervisor's
# verify-then-place seam: it never touches the live deploy path (which
# would destroy the rollback image). The placer verifies the sidecar with
# `shasum -c`, so the sidecar is the exact `<hash>  <basename>` line over
# the SIGNED bytes, and the card's name carries the same hash so a stale
# sidecar can never be matched to a fresh card by a glob readback.
#
# Usage: scripts/stage-card.sh [--skip-build] [discriminator ...]
#   discriminator  a string that must be present in the new card and
#                  absent from the running daemon image; each one is
#                  reported both ways so a stale build is refused here
#                  rather than after placement.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

STAGING="${CK_STAGING_DIR:-$HOME/.local/share/cortexkit/staging}"
DEPLOY="${CK_DEPLOY_PATH:-$HOME/.local/share/cortexkit/bin/ck-aft}"
SKIP_BUILD=0
DISCRIMINATORS=()
for arg in "$@"; do
  case "$arg" in
    --skip-build) SKIP_BUILD=1 ;;
    -h|--help) sed -n '2,/^set /p' "$0" | sed 's/^# \{0,1\}//' | head -n -1; exit 0 ;;
    *) DISCRIMINATORS+=("$arg") ;;
  esac
done

if [ -n "$(git status --porcelain)" ]; then
  echo "stage-card: working tree is dirty; a card must be cut from a committed tree" >&2
  git status --short >&2
  exit 2
fi
SHA="$(git rev-parse --short=12 HEAD)"

BUILD_START="$(date +%s)"
if [ "$SKIP_BUILD" -eq 0 ]; then
  echo "==> cargo build --release -p agent-file-tools (sha $SHA)"
  cargo build --release -p agent-file-tools --bin aft
fi
BIN="target/release/aft"
# Freshness is asserted, not assumed: a card cut from a binary older than
# this invocation is exactly how a regressed image reached the daemon once.
if [ "$SKIP_BUILD" -eq 0 ] && [ "$(stat -f %m "$BIN")" -lt "$BUILD_START" ]; then
  echo "stage-card: $BIN predates this build invocation; refusing to stage a stale binary" >&2
  exit 2
fi

mkdir -p "$STAGING"
TMP="$(mktemp "$STAGING/ck-aft.tmp.XXXXXX")"
cp "$BIN" "$TMP"
chmod 755 "$TMP"
if [ "$(uname -s)" = "Darwin" ]; then
  # The identifier is pinned to the deploy name so macOS grants keyed on it
  # survive across cards; the default identifier derives from content.
  codesign --force --sign - --identifier ck-aft "$TMP"
  codesign --verify --strict "$TMP"
fi
HASH="$(shasum -a 256 "$TMP" | awk '{print $1}')"
CARD="ck-aft.${HASH:0:16}"
mv "$TMP" "$STAGING/$CARD"
# The sidecar is written from the final name so `shasum -c` matches as-is.
(cd "$STAGING" && shasum -a 256 "$CARD" > "$CARD.sha256.postsign" && shasum -c "$CARD.sha256.postsign" >/dev/null)

VERSION="$("$STAGING/$CARD" --version 2>/dev/null | head -1)"
echo "==> card: $STAGING/$CARD"
echo "    sha256: $HASH"
echo "    self-report: $VERSION (source $SHA)"

if [ "${#DISCRIMINATORS[@]}" -gt 0 ]; then
  echo "==> discriminators (card / running image at $DEPLOY)"
  fail=0
  for d in "${DISCRIMINATORS[@]}"; do
    card_n="$(grep -a -c -F -- "$d" "$STAGING/$CARD" || true)"
    live_n=0
    [ -f "$DEPLOY" ] && live_n="$(grep -a -c -F -- "$d" "$DEPLOY" || true)"
    printf '    %-48s card=%s live=%s\n' "$d" "$card_n" "$live_n"
    if [ "$card_n" -eq 0 ]; then
      echo "    ^ absent from the card: this build does not carry it" >&2
      fail=1
    fi
  done
  [ "$fail" -eq 0 ] || exit 3
fi

echo "==> hand SUBC: $CARD ($HASH) in $STAGING"
