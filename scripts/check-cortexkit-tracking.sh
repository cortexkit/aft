#!/usr/bin/env bash
# Fail when Git tracks .cortexkit/ files outside the shared allowlist or
# build-server staging files.
#
# .cortexkit/ holds private CortexKit working files (drafts, reports, scratch,
# agent state). Only the shared parts are meant to be tracked: release-notes/,
# skills/, rules/ and WORKER.md, plus top-level project settings whose .jsonc
# names contain only lowercase letters, digits and hyphens. Ignore rules do not
# stop Git from carrying a file it already tracks, and a stray `!` rule can make
# a private file visible to Git.
# List every tracked path and fail closed on anything outside the allowlist.
#
# Usage: scripts/check-cortexkit-tracking.sh [repository-dir]
# Exits 0 when only allowlisted .cortexkit/ files and no .aft-ci/ build-server
# staging files are tracked, 1 when a forbidden file is tracked, and 2 when Git
# itself fails.
set -euo pipefail

repo="${1:-.}"

if ! listed="$(git -C "$repo" ls-files -- .cortexkit)"; then
  echo "check-cortexkit-tracking: git ls-files failed in $repo" >&2
  exit 2
fi

forbidden=""
while IFS= read -r path; do
  [ -n "$path" ] || continue
  if [[ "$path" =~ ^\.cortexkit/(release-notes/|skills/|rules/|WORKER\.md$|[a-z0-9-]+\.jsonc$) ]]; then
    continue
  fi
  forbidden+="$path"$'\n'
done <<< "$listed"

if [ -n "$forbidden" ]; then
  {
    echo "check-cortexkit-tracking: these tracked files are outside the .cortexkit/ allowlist:"
    printf '%s' "$forbidden" | sed 's/^/  /'
    echo "Only release-notes/, skills/, rules/, WORKER.md and top-level"
    echo "[a-z0-9-]+.jsonc project settings may be tracked there. Stop tracking"
    echo "the rest and keep the working copies with: git rm --cached <path>"
  } >&2
  exit 1
fi

if ! listed="$(git -C "$repo" ls-files -- .aft-ci)"; then
  echo "check-cortexkit-tracking: git ls-files failed in $repo" >&2
  exit 2
fi

if [ -n "$listed" ]; then
  {
    echo "files under .aft-ci/ are build-server staging and must never be committed: git rm --cached <path>"
    printf '%s\n' "$listed" | sed 's/^/  /'
  } >&2
  exit 1
fi

echo "check-cortexkit-tracking: only allowlisted .cortexkit/ files and no .aft-ci/ files are tracked"
