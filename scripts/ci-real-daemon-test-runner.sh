#!/usr/bin/env bash
set -euo pipefail
umask 077

# nextest invokes the Cargo runner separately for each test process. Isolate the
# embedded daemon too, not just the AFT children whose fixture sets HOME/XDG.
root="${AFT_REAL_DAEMON_RUN_ROOT:?set AFT_REAL_DAEMON_RUN_ROOT}"
mkdir -p "$root"
fixture="$(mktemp -d "$root/test.XXXXXXXX")"
trap 'rm -rf "$fixture"' EXIT

export HOME="$fixture/home"
export XDG_RUNTIME_DIR="$fixture/runtime"
export XDG_CONFIG_HOME="$fixture/config"
export XDG_DATA_HOME="$fixture/data"
export XDG_STATE_HOME="$fixture/state"
export XDG_CACHE_HOME="$fixture/cache"
export TMPDIR="$fixture/tmp"
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" \
  "$XDG_STATE_HOME" "$XDG_CACHE_HOME" "$TMPDIR"
chmod 700 "$XDG_RUNTIME_DIR"

"$@"
