#!/usr/bin/env bash
set -euo pipefail
docker version
artifact_dir="$PWD/target/prepush-opencode2"
mkdir -p "$artifact_dir"
cp "${CARGO_TARGET_DIR:-target}/stage/aft" "$artifact_dir/aft.real"
sha=$(git rev-parse HEAD)
printf '%s\n' '#!/bin/bash' 'set -euo pipefail' \
  'HERE="$(cd "$(dirname "$0")" && pwd)"' \
  'if [[ "${1:-}" == --version || "${1:-}" == -V ]]; then' \
  "  \"\$HERE/aft.real\" \"\$@\" | sed 's/\$/ ($sha)/'" \
  'else exec "$HERE/aft.real" "$@"; fi' > "$artifact_dir/aft"
chmod +x "$artifact_dir/aft" "$artifact_dir/aft.real"
digest=$(sha256sum "$artifact_dir/aft" | cut -d ' ' -f 1)
printf '{"git_sha":"%s","build_profile":"ci-release","sha256":"%s","source":"same-sha-artifact"}\n' \
  "$sha" "$digest" > "$artifact_dir/build-info.json"
export AFT_BINARY_PATH="$artifact_dir/aft"
export AFT_E2E_ARTIFACT_ROOT="$PWD/target/prepush-opencode2-artifacts"
export AFT_E2E_CONCURRENCY=4
timeout --signal=TERM --kill-after=30s 22m bash tests/docker/run-opencode2-test.sh
