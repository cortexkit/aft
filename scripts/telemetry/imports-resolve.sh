#!/usr/bin/env bash
# Resolve the immutable ruling and citation ledger before calculating search-quality scores.
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$script_dir/../../benchmarks/aft-search/imports_resolve.py" "$@"
