#!/usr/bin/env bash
# Reproduce the non-queue search gate; queue-member API planning stays on CI.
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/prepush/common.sh
prepush_init linux-search
trap 'prepush_abort $?' EXIT
prepush_step platform prepush_require_platform linux
prepush_step prerequisites bash scripts/prepush/require-tools.sh linux cargo rustc python3 git
prepush_step tool-versions bash -c 'set -euo pipefail; cargo --version; python3 --version; git --version'
prepush_step imports scripts/telemetry/imports-resolve.sh --spec benchmarks/aft-search/B1.md --snapshot 30d4a64f99b3b15fd88be6cf962fff4b3fe5ea17
prepush_step harness scripts/telemetry/cost-gate.sh --search-quality --self-test
prepush_step mode-tests python3 benchmarks/aft-search/ci_quality_mode.py --self-test
prepush_step binary cargo build --profile ci-storm --locked -p agent-file-tools --bin aft
export AFT_BINARY_PATH="${CARGO_TARGET_DIR:-$PWD/target}/ci-storm/aft"
prepush_step version "$AFT_BINARY_PATH" --version
prepush_step corpus python3 benchmarks/aft-search/provision_corpus.py
prepush_step evidence python3 benchmarks/aft-search/provision_evidence.py
base=$(git merge-base "${PREPUSH_BASE:-origin/main}" HEAD)
head=$(git rev-parse HEAD)
branch=${PREPUSH_BRANCH:-$(git branch --show-current)}
mode=$(python3 benchmarks/aft-search/ci_quality_mode.py --base "$base" --head "$head" 2>> "$lane_log")
[[ -n "$mode" ]] || { failures+=(mode-selector); exit 2; }
export AFT_SEARCH_NETWORK_POLICY=loopback-only
export AFT_SEARCH_CACHE_DIR="$PWD/target/prepush-empty-search-cache"
prepush_step quality scripts/telemetry/cost-gate.sh --search-quality --mode "$mode" \
  --base-ref "$base" --head "$head" --branch "$branch"
trap - EXIT
prepush_finish
