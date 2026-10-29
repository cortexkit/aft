#!/usr/bin/env bash
# One 16-vCPU allocation; keep CI's partitions and named retry profile.
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/prepush/common.sh
prepush_init linux-rust
trap 'prepush_abort $?' EXIT
prepush_step platform prepush_require_platform linux
export CI=1 AFT_SKIP_BOX_GATE=1
export CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only
export NEXTEST_TEST_THREADS=num-cpus AFT_UNIT_TEST_RUNNER=nextest
export AFT_GATE_SKIP_RELEASE_STORM=1 AFT_TEST_REQUIRE_RUST_ANALYZER=1
export AFT_GATE_LOG_DIR="$log_dir"
prepush_step tool-versions bash -c 'set -euo pipefail; cargo --version; rustc --version; cargo nextest --version; bun --version; printf "HOME=%s TMPDIR=%s\n" "$HOME" "${TMPDIR:-}"'
prepush_step sandbox-capability python3 scripts/prepush/linux-capabilities.py
prepush_step rust-analyzer rustup component add rust-analyzer
prepush_step dependencies bun install --frozen-lockfile
prepush_step pyright npm install --prefix target/prepush-pyright --no-save pyright@1.1.411
export PATH="$PWD/target/prepush-pyright/node_modules/.bin:$PATH"
prepush_step typescript-servers bash scripts/install-typescript-test-servers.sh "$PWD/target/prepush-typescript"
export AFT_TEST_TS7_NODE_MODULES="$PWD/target/prepush-typescript/ts7/node_modules"
export AFT_TEST_TS5_NODE_MODULES="$PWD/target/prepush-typescript/ts5/node_modules"
export AFT_NEXTEST_ARCHIVE_FILE="$PWD/target/prepush-linux-nextest.tar.zst"
prepush_step archive cargo nextest archive --workspace \
  -E '(kind(lib) | kind(bin) | kind(test)) - binary(=watcher_integration)' \
  --archive-file "$AFT_NEXTEST_ARCHIVE_FILE"
prepush_step inventory env AFT_UNIT_PARTITION_COUNT=3 python3 scripts/rust-test-inventory-proof.py
prepush_step tls cargo nextest run --archive-file "$AFT_NEXTEST_ARCHIVE_FILE" --profile gate -E 'kind(lib)' -- platform_verifier_tls_client_subprocess || true
prepush_step backtrace cargo nextest run --archive-file "$AFT_NEXTEST_ARCHIVE_FILE" --profile gate -E 'kind(lib)' -- panic_hook_preserves_default_backtrace_behavior_and_persists || true
for shard in 1 2 3; do
  prepush_step "unit-$shard" env AFT_GATE_PHASES=lib-shard AFT_NEXTEST_PARTITION="hash:$shard/3" \
    bash scripts/rust-test-gate.sh || true
  prepush_step "integration-$shard" env AFT_GATE_PHASES=nextest AFT_NEXTEST_PARTITION="hash:$shard/3" \
    bash scripts/rust-test-gate.sh || true
done
unset AFT_NEXTEST_ARCHIVE_FILE
prepush_step debug-binary cargo build -p agent-file-tools || true
prepush_step native-schemas bun packages/opencode-plugin/scripts/build-tool-schemas.ts --check || true
prepush_step native-drift bash scripts/prepush/native-drift.sh || true
prepush_step watcher-conformance env AFT_GATE_PHASES=watcher,conformance bash scripts/rust-test-gate.sh || true
prepush_step linux-contracts bash scripts/prepush/linux-contracts.sh || true
prepush_step storm-build cargo test --profile ci-storm --locked -p agent-file-tools --test integration --no-run --timings || true
prepush_step storm-inventory bash -c 'set -euo pipefail; cargo nextest list --profile default --cargo-profile ci-storm --locked -p agent-file-tools --test integration -E "test(subc_storm)" -T json > target/storm-inventory.json; python3 scripts/ci-storm-inventory.py --check-inventory target/storm-inventory.json' || true
prepush_step storm cargo nextest run --no-fail-fast --profile default --cargo-profile ci-storm --locked -p agent-file-tools --test integration -E 'test(subc_storm)' || true
prepush_step msrv-toolchain rustup toolchain install 1.92.0 --profile minimal || true
prepush_step msrv cargo +1.92.0 check --workspace --all-targets --locked || true
trap - EXIT
prepush_finish
