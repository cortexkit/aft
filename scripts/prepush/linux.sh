#!/usr/bin/env bash
# Run all Linux phases sequentially in the same 16-vCPU job.
set -euo pipefail
cd "$(dirname "$0")/../.."
source scripts/prepush/common.sh
prepush_init linux
trap 'prepush_abort $?' EXIT
prepush_step platform prepush_require_platform linux
prepush_step fast-compile bash scripts/prepush/fast-compile.sh
prepush_step rust bash scripts/prepush/linux-rust.sh || true
prepush_step typescript bash scripts/prepush/linux-typescript.sh || true
prepush_step search bash scripts/prepush/linux-search.sh || true
trap - EXIT
prepush_finish
