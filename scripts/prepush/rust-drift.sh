#!/usr/bin/env bash
# A Cargo filter matching zero tests is not a successful artifact check.
set -euo pipefail
filter=${1:?test filter required}
shift
unset AFT_STORAGE_DIR AFT_CACHE_DIR
home="$PWD/target/prepush-drift-home"
export XDG_DATA_HOME="$home/data" XDG_CONFIG_HOME="$home/config"
export XDG_STATE_HOME="$home/state" XDG_CACHE_HOME="$home/cache"
mkdir -p "$XDG_DATA_HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME"
out="${PREPUSH_LOG_DIR:-$PWD/.cortexkit/internal-docs/investigations/prepush}/drift-$filter.log"
mkdir -p "$(dirname "$out")"
rc=0
cargo test -p agent-file-tools --lib -- "$filter" "$@" > "$out" 2>&1 || rc=$?
python3 - "$out" "$rc" <<'PY'
import re
import sys
from pathlib import Path
text = Path(sys.argv[1]).read_text(errors="replace")
print(text, end="")
if int(sys.argv[2]):
    raise SystemExit(int(sys.argv[2]))
if not re.search(r"test result: ok\. [1-9]\d* passed", text):
    raise SystemExit("Artifact drift check ran zero tests; required generator/check is missing")
PY
