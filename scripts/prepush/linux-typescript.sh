#!/usr/bin/env bash
# Separate checkout allocation from linux-rust: never rebuild a live E2E binary.
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/prepush/common.sh
prepush_init linux-typescript
trap 'prepush_abort $?' EXIT
prepush_step platform prepush_require_platform linux
export CI=1
unset AFT_STORAGE_DIR AFT_CACHE_DIR
prepush_step prerequisites bash scripts/prepush/require-tools.sh linux bun node cargo npm rg docker timeout gh unzip
prepush_step tool-versions bash -c 'set -euo pipefail; bun --version; node --version; cargo --version; rg --version'
prepush_step docker-daemon bash scripts/prepush/require-tools.sh linux --probe 'Docker daemon' docker version
prepush_step dependencies bun install --frozen-lockfile
prepush_step bridge bun run --cwd packages/aft-bridge build
prepush_step pi bun run --cwd packages/pi-plugin build
prepush_step opencode bun run --cwd packages/opencode-plugin build
prepush_step typecheck bun run typecheck
prepush_step debug-binary cargo build -p agent-file-tools --bin aft
prepush_step subc-core bash -c 'set -euo pipefail; mkdir -p target; scripts/fetch-subc-core.sh > target/prepush-subc-path'
export SUBC_CORE_BIN
SUBC_CORE_BIN=$(<target/prepush-subc-path)
export SUBC_CORE_WIRE_VERSION=2
export AFT_CACHE_DIR="$PWD/target/prepush-typescript-cache"
export AFT_BINARY_PATH="${CARGO_TARGET_DIR:-$PWD/target}/debug/aft"
export AFT_SUBC_E2E_LOG_DIR="$log_dir/subc-e2e"
prepush_step unit bun run test:unit || true
prepush_step opencode-ndjson bash -c 'set -euo pipefail; cd packages/opencode-plugin; bun test --timeout 30000 src/__tests__/e2e/ --path-ignore-patterns "**/subc-parity.e2e.test.ts"' || true
prepush_step opencode-subc bash -c 'set -euo pipefail; cd packages/opencode-plugin; bun test --timeout 30000 src/__tests__/e2e/ --test-name-pattern "subc transport parity sweep"' || true
prepush_step pi-e2e bash -c 'set -euo pipefail; cd packages/pi-plugin; bun test --timeout 30000 src/__tests__/e2e/' || true
prepush_step bridge-e2e bash -c 'set -euo pipefail; cd packages/aft-bridge; bun test --timeout 30000 src/__tests__/e2e/' || true
prepush_step beta-pin bash scripts/prepush/beta-pin.sh || true
prepush_step stage-binary cargo build --profile stage -p agent-file-tools || true
export AFT_BINARY_PATH="${CARGO_TARGET_DIR:-$PWD/target}/stage/aft"
prepush_step pi-rpc bash -c 'set -euo pipefail; cd tests/pi-rpc; bun test' || true
prepush_step docker-e2e bash scripts/prepush/docker-e2e.sh || true
prepush_step opencode2-e2e bash scripts/prepush/opencode2-e2e.sh || true
trap - EXIT
prepush_finish
