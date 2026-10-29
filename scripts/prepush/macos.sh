#!/usr/bin/env bash
# Spend the fixed four-core Mac allocation on native behavior, not a second
# complete copy of Linux's platform-neutral inventory.
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/prepush/common.sh
prepush_init macos
trap 'prepush_abort $?' EXIT
prepush_step platform prepush_require_platform macos
export CI=1 AFT_SKIP_BOX_GATE=1 NEXTEST_TEST_THREADS=num-cpus
export CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only
export AFT_GATE_LOG_DIR="$log_dir" AFT_GATE_SKIP_RELEASE_STORM=1
prepush_step prerequisites bash scripts/prepush/require-tools.sh macos cargo cargo-nextest bun node npm python3 curl codesign
prepush_step tool-versions bash -c 'set -euo pipefail; cargo --version; cargo nextest --version; bun --version; sw_vers'
prepush_step timeout-tool python3 tests/macos-e2e/bin/timeout --version
prepush_step dependencies bun install --frozen-lockfile
prepush_step bridge bun run --cwd packages/aft-bridge build
prepush_step pi bun run --cwd packages/pi-plugin build
prepush_step opencode bun run --cwd packages/opencode-plugin build
prepush_step debug-binary cargo build --bin aft -p agent-file-tools --timings
prepush_step native-unit cargo nextest run --no-fail-fast --workspace --lib --profile gate \
  -E 'test(sandbox) | test(platform_verifier_tls_client_subprocess) | test(panic_hook_preserves_default_backtrace_behavior_and_persists)' || true
prepush_step native-jail cargo nextest run --no-fail-fast -p agent-file-tools --profile gate \
  --test worker_jail --test sandbox_launch_probe || true
prepush_step watcher-conformance env AFT_GATE_PHASES=watcher,conformance bash scripts/rust-test-gate.sh || true
prepush_step bash-timeout bash scripts/prepush/bash-timeout.sh || true
export AFT_BINARY_PATH="${CARGO_TARGET_DIR:-$PWD/target}/debug/aft"
export AFT_PLUGIN_DIST="$PWD/packages/opencode-plugin/dist"
prepush_step sign codesign --force --sign - "$AFT_BINARY_PATH" || true
prepush_step signature codesign --verify --verbose=2 "$AFT_BINARY_PATH" || true
prepush_step executable "$AFT_BINARY_PATH" --version || true
prepush_step native-e2e bash tests/macos-e2e/run.sh || true
trap - EXIT
prepush_finish
