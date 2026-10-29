#!/usr/bin/env bash
set -euo pipefail
python3 --version
# These are mocked runner tests, not native builds. Avoid Darwin's extra
# Gatekeeper warm-up Cargo call in the Cargo-only failure-collection fixture.
export AFT_GATE_NO_XPROTECT_REMEDIATION=1
python3 scripts/check-path-deps.py --self-test
bash scripts/check-secret-test-fake.test.sh
python3 scripts/test-ci-rust-quality.py
bash scripts/test-ci-sccache-start.sh
for test in workflow_gates ci_rustc_cache rust_gate_collection ci_macos_integration_archive \
  ci_windows_probes ci_storm_inventory cache_write_policy native_release_artifact \
  native_release_workflow merge_queue_workflow pr_search_descriptor landing_policy; do
  python3 -m unittest discover -s scripts/lib -p "test_$test.py" -v
done
bash scripts/train-push.test.sh
bash scripts/land-pr.test.sh
bash scripts/release-ci-gate.test.sh
python3 scripts/telemetry/test_cpu_precision.py
python3 scripts/telemetry/test_index_census_slow_lines.py
scripts/telemetry/cost-gate.sh --self-test
scripts/telemetry/cost-gate.sh --search-quality --self-test
python3 benchmarks/aft-search/ci_quality_mode.py --self-test
