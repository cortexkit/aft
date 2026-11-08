# Refuse a card whose source is missing changes the running card carries.
#
# A card built from a branch that never received the running card's changes
# silently undoes them when placed. That happened on 2026-11-07: three cards
# built from a long-lived branch dropped a fix the card before them had shipped,
# and nothing noticed for five hours. This gate finds the commit the running card
# was built from and checks that every change in it is also in the new card's
# source (by ancestry, or by an equal patch for cherry-picked commits).
#
# Changes the new card leaves out on purpose (a rollback, or a branch that
# replaced a change with its own version) are acknowledged by listing their
# commits in STAGE_ACK_MISSING (space-separated, at least 7 hex digits each),
# or with STAGE_ACK_MISSING=all.
#
# Callers: scripts/stage-card.sh. Expects to run inside the repository whose
# HEAD is the new card's source.

# The commit the running card was built from, or nothing when it can't be told.
# First from the sidecar stage-card.sh writes beside each card it stages (the
# staged card with the same sha256 as the deployed file); else from the commit
# id the build embeds (AFT_BUILD_GIT_SHA), when exactly one commit of this
# repository appears in the binary. String literals sit back to back in the
# binary, so the id can touch other hex digits: every 40-digit window of each
# hex run is checked against the repository's commits.
ck_aft_running_card_source() {
  local deploy="$1" staging="$2"
  [ -f "$deploy" ] || return 0
  local hash card
  hash="$(shasum -a 256 "$deploy" | awk '{print $1}')"
  card="$staging/ck-aft.${hash:0:16}"
  if [ -f "$card.source" ]; then
    head -1 "$card.source"
    return 0
  fi
  local found
  found="$(LC_ALL=C grep -a -o -E '[0-9a-f]{40,}' "$deploy" | sort -u |
    awk 'NR == FNR { commits[$1] = 1; next }
      { for (i = 1; i + 39 <= length($0); i++) { s = substr($0, i, 40); if (s in commits) print s } }' \
      <(git rev-list --all) - | sort -u)" || true
  if [ -n "$found" ] && [ "$(printf '%s\n' "$found" | wc -l | tr -d ' ')" -eq 1 ]; then
    echo "$found"
  fi
  return 0
}

# Whether STAGE_ACK_MISSING acknowledges commit $1.
ck_aft_missing_acknowledged() {
  local commit="$1" ack
  for ack in ${STAGE_ACK_MISSING:-}; do
    [ "$ack" = all ] && return 0
    [ "${#ack}" -ge 7 ] && case "$commit" in "$ack"*) return 0 ;; esac
  done
  return 1
}

# Checks HEAD against the running card's source. Prints what it found; returns
# nonzero when the new card would drop an unacknowledged change.
ck_aft_check_card_source() {
  local deploy="$1" staging="$2" caller="${3:-card-source}"
  if [ ! -f "$deploy" ]; then
    echo "    no running card at $deploy; nothing to compare"
    return 0
  fi
  local live
  live="$(ck_aft_running_card_source "$deploy" "$staging")"
  if [ -z "$live" ]; then
    if ck_aft_missing_acknowledged unknown; then
      echo "    can't tell which commit the running card was built from (acknowledged: STAGE_ACK_MISSING=all)"
      return 0
    fi
    echo "$caller: can't tell which commit the running card at $deploy was built from, so can't check this card keeps its changes; set STAGE_ACK_MISSING=all to stage anyway" >&2
    return 1
  fi
  if git merge-base --is-ancestor "$live" HEAD 2>/dev/null; then
    echo "    running card's source ${live:0:12} is in this card's history"
    return 0
  fi
  local missing commit refused=0
  missing="$(git cherry HEAD "$live" | awk '$1 == "+" { print $2 }')"
  if [ -z "$missing" ]; then
    echo "    running card's source ${live:0:12} is not an ancestor, but every one of its changes is in this card"
    return 0
  fi
  echo "    running card's source ${live:0:12}: changes this card lacks:"
  for commit in $missing; do
    if ck_aft_missing_acknowledged "$commit"; then
      echo "      ${commit:0:12} (acknowledged) $(git log -1 --format=%s "$commit")"
    else
      echo "      ${commit:0:12} $(git log -1 --format=%s "$commit")"
      refused=1
    fi
  done
  if [ "$refused" -ne 0 ]; then
    echo "$caller: this card would undo changes the running card carries; bring them in, or list the commits left out on purpose in STAGE_ACK_MISSING" >&2
    return 1
  fi
  return 0
}
