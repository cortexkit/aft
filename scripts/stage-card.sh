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
# Never write the dSYM to "$BIN.dSYM": cargo owns that name as a symlink
# into deps/ and unlinks it on the next build, which fails with EPERM once a
# real bundle sits there (unlink on a directory) and breaks every later
# `cargo build --release` in the checkout (2026-09-18). The generated bundle
# lives beside the binary under a name cargo never touches.
DSYM="target/release/ck-aft-card.dSYM"
if [ "$(uname -s)" = "Darwin" ]; then
  # A card cut from a published release asset (--skip-build) is stripped;
  # its dSYM is the one the release shipped beside it, unpacked to
  # $BIN.dSYM by the operator. Regenerating from stripped bytes would mint
  # a dSYM whose UUID does not match the image and fail the check below.
  if [ "$SKIP_BUILD" -eq 1 ] && [ -d "$BIN.dSYM" ] && [ ! -L "$BIN.dSYM" ]; then
    echo "==> using existing $BIN.dSYM"
    DSYM="$BIN.dSYM"
  elif [ "$SKIP_BUILD" -eq 0 ] && [ -L "$BIN.dSYM" ] && [ -d "$BIN.dSYM" ]; then
    # The release profile packs debug info into cargo's own dSYM and links
    # the binary without a debug map, so running dsymutil on the binary
    # mints a bundle with the right UUID and no DWARF (1.8 MB instead of
    # ~200 MB): it passes the UUID check and symbolicates nothing. Copy
    # cargo's bundle instead (the symlink points into deps/).
    rm -rf "$DSYM"
    ditto "$BIN.dSYM/" "$DSYM"
  else
    rm -rf "$DSYM"
    dsymutil "$BIN" -o "$DSYM"
  fi
fi
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

  IMAGE_UUID="$(dwarfdump --uuid "$TMP" | awk 'NR == 1 { gsub(/-/, "", $2); print toupper($2) }')"
  DSYM_UUID="$(dwarfdump --uuid "$DSYM" | awk 'NR == 1 { gsub(/-/, "", $2); print toupper($2) }')"
  if [ -z "$IMAGE_UUID" ] || [ -z "$DSYM_UUID" ] || [ "$IMAGE_UUID" != "$DSYM_UUID" ]; then
    echo "stage-card: dSYM mismatch: card UUID ${IMAGE_UUID:-missing}, dSYM UUID ${DSYM_UUID:-missing}" >&2
    exit 2
  fi
  # A matching UUID does not prove the bundle carries DWARF. Symbolicate the
  # executable's entry point (LC_MAIN) through it: a bundle with debug info
  # names it `main`, an empty one echoes the raw address back.
  DSYM_DWARF="$(find "$DSYM/Contents/Resources/DWARF" -type f | head -1)"
  ENTRY_OFF="$(otool -l "$TMP" | awk '/LC_MAIN/ { found = 1 } found && /entryoff/ { print $2; exit }')"
  if [ -z "$DSYM_DWARF" ] || [ -z "$ENTRY_OFF" ] ||
    ! atos -o "$DSYM_DWARF" -arch arm64 "$(printf '0x%x' $((0x100000000 + ENTRY_OFF)))" 2>/dev/null |
    grep -q '^main '; then
    echo "stage-card: dSYM $DSYM has no usable debug info (the entry point does not symbolicate)" >&2
    exit 2
  fi
  DSYM_ROOT="${AFT_DSYM_DIR:-$HOME/.local/share/cortexkit/aft/dsym}"
  mkdir -p "$DSYM_ROOT"
  DSYM_DEST="$DSYM_ROOT/$DSYM_UUID"
  DSYM_TMP="$(mktemp -d "$DSYM_ROOT/.${DSYM_UUID}.tmp.XXXXXX")"
  ditto "$DSYM" "$DSYM_TMP/aft.dSYM"
  rm -rf "$DSYM_DEST"
  mv "$DSYM_TMP" "$DSYM_DEST"
  echo "    dSYM: $DSYM_DEST/aft.dSYM (UUID $DSYM_UUID)"
fi
HASH="$(shasum -a 256 "$TMP" | awk '{print $1}')"
CARD="ck-aft.${HASH:0:16}"
mv "$TMP" "$STAGING/$CARD"
# The sidecar is written from the final name so `shasum -c` matches as-is.
(cd "$STAGING" && shasum -a 256 "$CARD" > "$CARD.sha256.postsign" && shasum -c "$CARD.sha256.postsign" >/dev/null)
# Owner declaration of the current card, so the placement gate reads it as
# declared rather than inferring the newest card from mtimes. One shasum-shaped
# line plus a UTC stamp; the gate treats it as inert if absent.
printf '%s  %s  %s\n' "$HASH" "$CARD" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$STAGING/ck-aft.current"

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
