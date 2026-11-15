#!/usr/bin/env bash
# Fail when Git tracks ignored .cortexkit/ files or build-server staging files.
#
# .cortexkit/ holds private CortexKit working files (drafts, reports, scratch,
# agent state). Only the shared parts are meant to be tracked: release-notes/,
# skills/, rules/ and WORKER.md, plus the project files the repository's
# .gitignore re-includes by name. Ignore rules do not stop Git from carrying a
# file it already tracks (one committed before the rule existed, or added with
# `git add -f`), so such a file keeps being pushed. This check lists every
# tracked file that the ignore rules exclude, however it got tracked.
#
# Usage: scripts/check-cortexkit-tracking.sh [repository-dir]
# Exits 0 when no ignored .cortexkit/ files or .aft-ci/ files are tracked,
# 1 when any are tracked, and 2 when Git itself fails.
set -euo pipefail

repo="${1:-.}"

if ! listed="$(git -C "$repo" ls-files -ci --exclude-standard -- .cortexkit)"; then
  echo "check-cortexkit-tracking: git ls-files failed in $repo" >&2
  exit 2
fi

if [ -n "$listed" ]; then
  {
    echo "check-cortexkit-tracking: these files under .cortexkit/ are tracked but ignored:"
    printf '%s\n' "$listed" | sed 's/^/  /'
    echo "Only release-notes/, skills/, rules/, WORKER.md and the files .gitignore"
    echo "re-includes may be tracked there. Stop tracking the rest and keep the"
    echo "working copies with: git rm --cached <path>"
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

echo "check-cortexkit-tracking: no forbidden files tracked under .cortexkit/ or .aft-ci/"
