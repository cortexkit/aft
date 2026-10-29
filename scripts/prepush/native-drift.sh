#!/usr/bin/env bash
set -euo pipefail
# Compare the write-only catalog generator byte for byte, then restore inputs.
python3 scripts/prepush/check-generated.py \
  crates/aft/tests/fixtures/tool_provider_catalog.json \
  crates/aft/tests/fixtures/tool_provider_catalog_presets.json \
  -- bash scripts/prepush/rust-drift.sh regenerate_tool_provider_catalog --ignored
bash scripts/prepush/rust-drift.sh carrier_samples_match_wire
bash scripts/prepush/rust-drift.sh rich_ui_samples
