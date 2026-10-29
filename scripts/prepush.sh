#!/usr/bin/env bash
# Run the cheap checks here; launch the full lanes with runon (see WORKER.md).
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/prepush/common.sh
if [[ $# == 1 && "${1:-}" == --print-lanes ]]; then
  exec python3 scripts/prepush/lanes.py
fi
if [[ "${1:-fast}" != fast || $# -gt 1 ]]; then
  echo 'Usage: scripts/prepush.sh [fast | --print-lanes]'
  echo 'Runs local fast and prints numbered remote commands; this script never submits jobs.'
  echo 'Remote lanes: scripts/prepush/{linux,windows,macos}.sh'
  exit 2
fi
prepush_init fast
trap 'prepush_abort $?' EXIT
mkdir -p target/scratch/prepush-temp
export TMPDIR="$PWD/target/scratch/prepush-temp"
prepush_step versions bash -c 'set -euo pipefail; cargo --version; cargo fmt --version; bun --version; node --version; python3 --version'
prepush_step rust-format cargo fmt --all -- --check
prepush_step bun-lock bun install --frozen-lockfile
prepush_step lint python3 scripts/prepush/lint.py
prepush_step cargo-lock bash -c 'set -euo pipefail; mkdir -p "${CARGO_TARGET_DIR:-target}"; cargo metadata --locked --format-version 1 > "${CARGO_TARGET_DIR:-target}/prepush-metadata.json"; python3 -c '\''import json, sys; data=json.load(open(sys.argv[1])); count=len(data["packages"]); assert count > 0; print("Locked Cargo metadata: %s packages, %s workspace members" % (count, len(data["workspace_members"])))'\'' "${CARGO_TARGET_DIR:-target}/prepush-metadata.json"'
prepush_step bridge bun run --cwd packages/aft-bridge build
prepush_step typecheck bun run typecheck
prepush_step generated bash scripts/prepush/generated.sh
prepush_step tracking bash scripts/check-cortexkit-tracking.sh
prepush_step release-notes bash -c 'set -euo pipefail; shopt -s nullglob; files=(.alfonso/release-notes/v*.md .alfonso/release-notes/next/*.md); node scripts/check-release-notes.mjs "${files[@]}"'
prepush_step search-descriptor python3 benchmarks/aft-search/search_quality.py --mode check-descriptor \
  --base-ref "${PREPUSH_BASE:-origin/main}" --head HEAD --branch "${PREPUSH_BRANCH:-$(git branch --show-current)}"
prepush_step landing-identity python3 scripts/landing-policy.py \
  --base "$(git merge-base "${PREPUSH_BASE:-origin/main}" HEAD)" --head "$(git rev-parse HEAD)" --branch "${PREPUSH_BRANCH:-$(git branch --show-current)}"
prepush_step fixture-attributes python3 scripts/prepush/check-fixture-attributes.py
prepush_step feature-inventory python3 scripts/prepush/check-feature-matrix.py
prepush_step path-dependencies python3 scripts/check-path-deps.py
prepush_step secret-fake bash scripts/check-secret-test-fake.sh
prepush_step rust-ci-policy python3 scripts/test-ci-rust-quality.py
prepush_step prepush-tests python3 -m unittest discover -s scripts/prepush -p 'test_*.py' -v
prepush_step package-shapes node scripts/validate-packages.mjs
prepush_step publication-inputs node scripts/release-gate-v049.mjs --check-inputs
trap - EXIT
prepush_finish
python3 scripts/prepush/lanes.py
