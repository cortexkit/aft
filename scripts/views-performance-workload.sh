#!/usr/bin/env bash
# Keep all arms sequential on the same host allocation. Extra arguments select
# a bounded acquisition or a fixed real fastembed model without changing pins.
set -euo pipefail
exec python3 "$(dirname "$0")/views-performance-gate.py" --roots 1 5 15 "$@"
