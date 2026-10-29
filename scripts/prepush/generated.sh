#!/usr/bin/env bash
set -euo pipefail
# Native hashline, catalog-reply and carrier-wire checks run in the Rust lanes;
# their generators compile Rust and must never build on the operator's Mac.
mkdir -p target/scratch/prepush-temp
export TMPDIR="$PWD/target/scratch/prepush-temp"
bun packages/opencode-plugin/scripts/build-tool-schemas.ts --check-source-only
python3 scripts/prepush/check-generated.py crates/aft/tests/fixtures/config_parity -- bun scripts/capture-config-parity.ts
bun spec/feature-config/check-parity-baseline.ts
