#!/usr/bin/env bash
# Dispatch nightly OSS cost gates. The default runner compares two index-cost
# samples across the fixed repository matrix; --exact-recall checks deterministic
# sentence and token-pair retrieval against its checked-in baseline.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ "${1:-}" == "--exact-recall" ]]; then
  shift
  exec python3 "$script_dir/../../benchmarks/aft-search/run_exact_recall.py" "$@"
fi

exec python3 "$script_dir/cost-gate.py" "$@"
