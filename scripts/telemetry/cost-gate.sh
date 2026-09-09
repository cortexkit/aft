#!/usr/bin/env bash
# Dispatch index-cost and the independent search-quality benchmark families.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
benchmark_dir="$script_dir/../../benchmarks/aft-search"

case "${1:-}" in
  --exact-recall)
    shift
    exec python3 "$benchmark_dir/run_exact_recall.py" "$@"
    ;;
  --concept-recall)
    shift
    exec python3 "$benchmark_dir/run_concept_recall.py" "$@"
    ;;
  --real-query)
    shift
    exec python3 "$benchmark_dir/run_real_query.py" "$@"
    ;;
  --search-quality)
    shift
    exec python3 "$benchmark_dir/run_search_quality.py" "$@"
    ;;
  --record-vectors)
    shift
    exec python3 "$benchmark_dir/record_vectors.py" "$@"
    ;;
esac

exec python3 "$script_dir/cost-gate.py" "$@"
