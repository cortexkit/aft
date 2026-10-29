#!/usr/bin/env bash
# Run the cheap checks here; launch the full lanes with runon (see WORKER.md).
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/prepush/common.sh
if [[ "${1:-}" != fast || $# != 1 ]]; then
  echo 'Usage: scripts/prepush.sh fast'
  echo 'Server fast phase: scripts/prepush/fast-compile.sh (linux,4c)'
  echo 'Full lanes: scripts/prepush/{linux-rust,linux-typescript,linux-search,windows,macos}.sh'
  exit 2
fi
prepush_init fast
trap 'prepush_abort $?' EXIT
mkdir -p target/scratch/prepush-temp
export TMPDIR="$PWD/target/scratch/prepush-temp"
prepush_step versions bash -c 'set -euo pipefail; cargo fmt --version; bun --version; node --version; python3 --version'
prepush_step rust-format cargo fmt --all -- --check
prepush_step bun-lock bun install --frozen-lockfile
prepush_step biome-version ./node_modules/.bin/biome --version
prepush_step lint bun run lint
prepush_step cargo-lock bash -c 'set -euo pipefail; mkdir -p "${CARGO_TARGET_DIR:-target}"; cargo metadata --locked --format-version 1 > "${CARGO_TARGET_DIR:-target}/prepush-metadata.json"'
prepush_step bridge bun run --cwd packages/aft-bridge build
prepush_step typecheck bun run typecheck
prepush_step generated bash scripts/prepush/generated.sh
prepush_step tracking bash scripts/check-cortexkit-tracking.sh
prepush_step release-notes bash -c 'set -euo pipefail; shopt -s nullglob; files=(.alfonso/release-notes/v*.md .alfonso/release-notes/next/*.md); node scripts/check-release-notes.mjs "${files[@]}"'
prepush_step search-descriptor python3 benchmarks/aft-search/search_quality.py --mode check-descriptor \
  --base-ref "${PREPUSH_BASE:-origin/main}" --head HEAD --branch "$(git branch --show-current)"
prepush_step landing-identity python3 scripts/landing-policy.py \
  --base "$(git merge-base "${PREPUSH_BASE:-origin/main}" HEAD)" --head "$(git rev-parse HEAD)" --branch "$(git branch --show-current)"
prepush_step fixture-attributes python3 scripts/prepush/check-fixture-attributes.py
prepush_step feature-inventory python3 scripts/prepush/check-feature-matrix.py
prepush_step path-dependencies python3 scripts/check-path-deps.py
prepush_step path-dependency-tests python3 scripts/check-path-deps.py --self-test
prepush_step secret-fake-tests bash scripts/check-secret-test-fake.test.sh
prepush_step secret-fake bash scripts/check-secret-test-fake.sh
prepush_step rust-ci-policy python3 scripts/test-ci-rust-quality.py
prepush_step prepush-tests python3 -m unittest discover -s scripts/prepush -p 'test_*.py' -v
prepush_step package-shapes node scripts/validate-packages.mjs
prepush_step publication-inputs node scripts/release-gate-v049.mjs --check-inputs
trap - EXIT
prepush_finish
