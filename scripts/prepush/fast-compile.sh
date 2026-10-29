#!/usr/bin/env bash
# Mandatory server half of the fast phase: never compile on the user's Mac.
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/prepush/common.sh
prepush_init fast-compile
trap 'prepush_abort $?' EXIT
prepush_step platform prepush_require_platform linux
prepush_step versions bash -c 'set -euo pipefail; cargo --version; rustc --version; python3 --version'
prepush_step feature-inventory python3 scripts/prepush/check-feature-matrix.py
prepush_step linux-default cargo check --workspace --tests --locked
for features in test-timing-hooks test-sandbox-launcher test-conformance-result-inputs \
  test-sandbox-launcher,test-conformance-result-inputs \
  test-timing-hooks,test-sandbox-launcher,test-conformance-result-inputs; do
  prepush_step "linux-$features" cargo check -p agent-file-tools --tests --locked --features "$features"
done
# Linux cross-checks catch Windows cfg/type errors; native MSVC runtime tests
# and strict warnings checks remain mandatory in the Windows lane.
prepush_step windows-target rustup target add x86_64-pc-windows-gnu
prepush_step windows-default env RUSTFLAGS='-D warnings' cargo check --workspace --tests --locked --target x86_64-pc-windows-gnu
prepush_step windows-test-timing-hooks env RUSTFLAGS='-D warnings' cargo check -p agent-file-tools --tests --locked \
  --features test-timing-hooks,test-sandbox-launcher,test-conformance-result-inputs --target x86_64-pc-windows-gnu
prepush_step workflow-guards bash scripts/prepush/workflow-guards.sh
trap - EXIT
prepush_finish
