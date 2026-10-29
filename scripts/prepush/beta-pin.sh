#!/usr/bin/env bash
set -euo pipefail
if git diff --quiet "${PREPUSH_BASE:-origin/main}...HEAD" -- \
  packages/opencode-plugin/test/matrix packages/opencode-plugin/test/load-matrix \
  .github/opencode-version.txt .github/workflows/beta-pin-gate.yml; then
  echo 'Beta-pin matrix not applicable: no workflow-trigger paths changed'
  exit 0
fi
isolation="$PWD/target/prepush-beta-pin"
mkdir -p "$isolation"/{home,config,data,state,cache}
export HOME="$isolation/home" XDG_CONFIG_HOME="$isolation/config" XDG_DATA_HOME="$isolation/data"
export XDG_STATE_HOME="$isolation/state" XDG_CACHE_HOME="$isolation/cache"
export OPENCODE_FLAGS=--standalone AFT_LOAD_MATRIX_ALLOW_LIVE_OPERATOR=0
bun test ./packages/opencode-plugin/test/matrix/acceptance-matrix.test.ts ./packages/opencode-plugin/test/load-matrix/load-matrix.ts
