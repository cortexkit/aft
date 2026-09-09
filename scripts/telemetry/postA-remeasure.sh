#!/usr/bin/env bash
# Offline, non-CI post-release estimator; its outputs never feed cost-gate.
set -euo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$script_dir/../../benchmarks/aft-search/post_release_remeasure.py" "$@"
