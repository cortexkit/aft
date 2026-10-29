#!/usr/bin/env bash
# Git Bash, native MSVC tests: no cross-compile presented as a runtime pass.
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/prepush/common.sh
prepush_init windows
trap 'prepush_abort $?' EXIT
prepush_step platform prepush_require_platform windows
export CI=1 AFT_SKIP_BOX_GATE=1 AFT_TEST_DIAG=1
export CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only
export NEXTEST_TEST_THREADS=4 AFT_UNIT_TEST_RUNNER=nextest AFT_GATE_SKIP_RELEASE_STORM=1
export AFT_GATE_LOG_DIR="$log_dir"
prepush_step tool-versions bash -c 'set -euo pipefail; cargo --version; rustc --version; cargo nextest --version; bun --version'
prepush_step python-version python -c 'import sys; print(sys.version); assert sys.version_info >= (3, 14), "CI archive probes require Python 3.14+"'
prepush_step storage-preflight bash scripts/windows-gate.sh --filter gate_hermeticity_tests::gate_resolves_every_user_config_and_state_path_under_the_gate_homes
export AFT_NEXTEST_ARCHIVE_FILE="$PWD/target/prepush-windows-nextest.tar.zst"
prepush_step archive cargo nextest archive --workspace \
  -E '(kind(lib) | kind(bin) | kind(test)) - binary(=watcher_integration)' \
  --archive-file "$AFT_NEXTEST_ARCHIVE_FILE"
prepush_step tls python scripts/ci-windows-probes.py --archive-file "$AFT_NEXTEST_ARCHIVE_FILE" --probe tls || true
prepush_step backtrace python scripts/ci-windows-probes.py --archive-file "$AFT_NEXTEST_ARCHIVE_FILE" --probe backtrace || true
prepush_step inventory env AFT_UNIT_PARTITION_COUNT=6 python3 scripts/rust-test-inventory-proof.py
for shard in 1 2 3 4 5 6; do
  prepush_step "unit-$shard" env AFT_GATE_PHASES=lib-shard AFT_NEXTEST_PARTITION="hash:$shard/6" \
    bash scripts/rust-test-gate.sh || true
done
for shard in 1 2 3; do
  prepush_step "integration-$shard" env AFT_GATE_PHASES=nextest AFT_NEXTEST_PARTITION="hash:$shard/3" \
    bash scripts/rust-test-gate.sh || true
done
unset AFT_NEXTEST_ARCHIVE_FILE
prepush_step watcher-conformance env AFT_GATE_PHASES=watcher,conformance bash scripts/rust-test-gate.sh || true
prepush_step deny-warnings env RUSTFLAGS='-D warnings' cargo check --workspace --all-targets || true
prepush_step provider-deny-warnings env RUSTFLAGS='-D warnings' cargo check -p agent-file-tools --features test-timing-hooks --test rest || true
prepush_step dependencies bun install --frozen-lockfile
prepush_step bridge bun run --cwd packages/aft-bridge build
prepush_step pi bun run --cwd packages/pi-plugin build
prepush_step opencode bun run --cwd packages/opencode-plugin build
prepush_step debug-binary cargo build -p agent-file-tools || true
export AFT_CACHE_DIR="$PWD/target/prepush-windows-cache"
prepush_step bash-e2e bash -c 'set -euo pipefail; cd packages/opencode-plugin; bun test src/__tests__/e2e/bash.test.ts' || true
prepush_step stage-binary cargo build --profile stage --bin aft --target x86_64-pc-windows-msvc -p agent-file-tools --timings || true
AFT_BINARY_PATH="$(cygpath -w "${CARGO_TARGET_DIR:-$PWD/target}/x86_64-pc-windows-msvc/stage/aft.exe")"
AFT_PLUGIN_DIST="$(cygpath -w "$PWD/packages/opencode-plugin/dist")"
export AFT_BINARY_PATH AFT_PLUGIN_DIST
prepush_step native-e2e pwsh -NoProfile -ExecutionPolicy Bypass -File tests/windows-e2e/run.ps1 || true
trap - EXIT
prepush_finish
