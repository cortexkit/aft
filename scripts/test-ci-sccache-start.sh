#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mkdir -p "$repo_root/target/scratch"
scratch=$(mktemp -d "$repo_root/target/scratch/ci-sccache.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/bin" "$scratch/runner temp"

cat > "$scratch/bin/sccache" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[[ "$*" == "--start-server" ]]
[[ $(< "$SCCACHE_CONF") == "$SCCACHE_TEST_EXPECTED_CONF" ]]
attempt=$(< "$SCCACHE_TEST_ATTEMPTS")
attempt=$((attempt + 1))
printf '%s\n' "$attempt" > "$SCCACHE_TEST_ATTEMPTS"
if (( attempt <= SCCACHE_TEST_FAILURES )); then
    echo 'sccache: error: Timed out waiting for server startup.' >&2
    exit 2
fi
STUB
cat > "$scratch/bin/sleep" <<'STUB'
#!/usr/bin/env bash
exit 0
STUB
chmod +x "$scratch/bin/sccache" "$scratch/bin/sleep"

run_startup() {
    local failures=$1
    # An explicit config path stands for a runner that already chose its sccache settings.
    local explicit_conf=${2:-}
    export SCCACHE_TEST_EXPECTED_CONF='server_startup_timeout_ms = 30000'
    if [[ -n "$explicit_conf" ]]; then
        SCCACHE_TEST_EXPECTED_CONF=$(< "$explicit_conf")
    fi
    export SCCACHE_TEST_ATTEMPTS="$scratch/attempts"
    export SCCACHE_TEST_FAILURES="$failures"
    export GITHUB_ENV="$scratch/github env"
    printf '0\n' > "$SCCACHE_TEST_ATTEMPTS"
    : > "$GITHUB_ENV"
    step_status=0
    PATH="$scratch/bin:$PATH" RUSTC_WRAPPER=sccache SCCACHE_CONF="$explicit_conf" RUNNER_TEMP="$scratch/runner temp" \
        bash "$repo_root/scripts/ci-sccache-start.sh" > "$scratch/output" 2>&1 || step_status=$?
}

apply_job_env() {
    local name value
    while IFS='=' read -r name value; do
        export "$name=$value"
    done < "$GITHUB_ENV"
}

failed_startup_runs_uncached() {
    run_startup 99
    [[ "$step_status" == 0 ]] || { echo "startup step exited $step_status, expected 0"; return 1; }
    [[ $(< "$SCCACHE_TEST_ATTEMPTS") == 3 ]] || { echo 'expected exactly three startup attempts'; return 1; }
    local RUSTC_WRAPPER=sccache
    # GitHub applies $GITHUB_ENV before the next step; Cargo treats an empty wrapper as disabled.
    apply_job_env
    [[ -z "$RUSTC_WRAPPER" ]] || { echo 'next step would still use sccache'; return 1; }
    local warnings=0 line
    while IFS= read -r line; do
        if [[ "$line" == ::warning::*uncached* ]]; then
            warnings=$((warnings + 1))
        fi
    done < "$scratch/output"
    [[ "$warnings" == 1 ]] || { echo "expected one uncached warning, got $warnings"; return 1; }
}

successful_startup_keeps_wrapper() {
    run_startup 0
    [[ "$step_status" == 0 && $(< "$SCCACHE_TEST_ATTEMPTS") == 1 ]] || return 1
    local RUSTC_WRAPPER=sccache
    apply_job_env
    [[ "$RUSTC_WRAPPER" == sccache && $(< "$SCCACHE_CONF") == 'server_startup_timeout_ms = 30000' ]]
}

transient_failure_recovers() {
    run_startup 2
    [[ "$step_status" == 0 && $(< "$SCCACHE_TEST_ATTEMPTS") == 3 ]] || return 1
    local RUSTC_WRAPPER=sccache
    apply_job_env
    [[ "$RUSTC_WRAPPER" == sccache ]]
}

explicit_config_is_kept() {
    local custom="$scratch/custom sccache.toml"
    printf '[cache.disk]\ndir = "/runner/cache"\n' > "$custom"
    local before
    before=$(< "$custom")
    rm -f "$scratch/runner temp/aft-sccache.toml"
    run_startup 0 "$custom"
    [[ "$step_status" == 0 && $(< "$SCCACHE_TEST_ATTEMPTS") == 1 ]] || { echo 'startup with an explicit config failed'; return 1; }
    [[ $(< "$custom") == "$before" ]] || { echo 'explicit config file was changed'; return 1; }
    [[ ! -e "$scratch/runner temp/aft-sccache.toml" ]] || { echo 'a replacement config was written'; return 1; }
    local entry
    while IFS= read -r entry; do
        [[ "$entry" != SCCACHE_CONF=* ]] || { echo "job env replaces the config path: $entry"; return 1; }
    done < "$GITHUB_ENV"
}

passed=0
failed=0
for test in failed_startup_runs_uncached successful_startup_keeps_wrapper transient_failure_recovers explicit_config_is_kept; do
    if [[ $# == 0 || "$1" == "$test" ]]; then
        if "$test"; then
            echo "PASS $test"
            passed=$((passed + 1))
        else
            echo "FAIL $test"
            failed=$((failed + 1))
        fi
    fi
done
echo "$passed passed; $failed failed"
(( passed > 0 && failed == 0 ))
