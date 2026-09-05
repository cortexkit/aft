#!/usr/bin/env bash
# Run the fixed, twice-sampled nightly OSS cost gate. The Python helper owns
# cloning, NDJSON-matrix invocation, structured metric comparison, and reports.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ "${1:-}" == "--exact-recall" ]]; then
  shift
  exec python3 "$script_dir/../../benchmarks/aft-search/run_exact_recall.py" "$@"
fi

exec python3 "$script_dir/cost-gate.py" "$@"
