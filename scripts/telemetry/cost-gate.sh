#!/usr/bin/env bash
# Dispatch index-cost and the independent search-quality benchmark families.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
benchmark_dir="$script_dir/../../benchmarks/aft-search"

case "${1:-}" in
  --exact-recall)
    shift
    # run_exact_recall validates a provisioning record that only provision_corpus
    # writes. Its own clone path leaves no record, so a fresh checkout fails with
    # corpus_missing:provision_record unless we provision first -- the same shape
    # as --search-quality provisioning its evidence tree below.
    python3 "$benchmark_dir/provision_corpus.py"
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
    quality_mode="evaluate"
    quality_arguments=("$@")
    for ((index = 0; index < ${#quality_arguments[@]}; index++)); do
      argument="${quality_arguments[$index]}"
      if [[ "$argument" == "--self-test" ]]; then
        quality_mode="self-test"
      elif [[ "$argument" == --mode=* ]]; then
        quality_mode="${argument#--mode=}"
      elif [[ "$argument" == "--mode" && $((index + 1)) -lt ${#quality_arguments[@]} ]]; then
        quality_mode="${quality_arguments[$((index + 1))]}"
      fi
    done
    if [[ "$quality_mode" != "self-test" ]]; then
      python3 "$benchmark_dir/provision_evidence.py"
    fi
    exec python3 "$benchmark_dir/run_search_quality.py" "${quality_arguments[@]}"
    ;;
  --record-vectors)
    shift
    exec python3 "$benchmark_dir/record_vectors.py" "$@"
    ;;
esac

exec python3 "$script_dir/cost-gate.py" "$@"
