#!/usr/bin/env bash
set -euo pipefail

# sccache v0.19.0 has a file-only startup timeout (milliseconds), not an env knob.
# Give slow runners 30 seconds per attempt without changing an explicit config.
# https://github.com/mozilla/sccache/blob/v0.19.0/docs/Configuration.md
if [[ -z "${SCCACHE_CONF:-}" ]]; then
    export SCCACHE_CONF="$RUNNER_TEMP/aft-sccache.toml"
    printf 'server_startup_timeout_ms = 30000\n' > "$SCCACHE_CONF"
    printf 'SCCACHE_CONF=%s\n' "$SCCACHE_CONF" >> "$GITHUB_ENV"
fi

for attempt in 1 2 3; do
    echo "Starting compiler cache (attempt $attempt/3)"
    if sccache --start-server; then
        exit 0
    fi
    if [[ "$attempt" != 3 ]]; then
        sleep 2
    fi
done

# An empty wrapper disables Cargo's job-level RUSTC_WRAPPER in subsequent steps.
unset RUSTC_WRAPPER
printf 'RUSTC_WRAPPER=\n' >> "$GITHUB_ENV"
echo '::warning::sccache server did not start after 3 attempts; this job runs uncached.'
