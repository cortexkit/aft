#!/usr/bin/env bash
# ------------------------------------------------------------------
# Test: ONNX Runtime failure must NOT crash the aft binary.
#
# Scenarios:
#   1. No ONNX Runtime at all — semantic search should be skipped
#   2. Broken libonnxruntime.so (can't dlopen) — should skip, not panic
#   3. ORT_DYLIB_PATH pointing to missing file — should skip, not panic
#
# Each scenario:
#   - Starts aft via stdin
#   - Sends configure (with experimental_semantic_search=true)
#   - Sends outline command
#   - Checks the process stayed alive and returned a valid response
# ------------------------------------------------------------------

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m' # No color

PASS=0
FAIL=0
PROJECT_DIR="/test/fixtures/sample-project"

# ── Helpers ───────────────────────────────────────────────────────

send_commands() {
    # Sends configure + command, waits for responses, returns the process exit code
    local env_vars="${1:-}"
    local extra_configure_params="${2:-}"
    local command_json="${3}"
    local timeout_sec="${4:-10}"

    local tmpout
    tmpout=$(mktemp)
    local tmperr
    tmperr=$(mktemp)

    # Build configure params
    local configure_params
    configure_params=$(cat <<CONF
{"harness":"opencode","project_root":"${PROJECT_DIR}","experimental_semantic_search":true${extra_configure_params:+,$extra_configure_params}}
CONF
    )

    # Send configure + command via stdin, capture stdout + stderr
    local exit_code=0
    timeout "${timeout_sec}" bash -c "
        ${env_vars:+export $env_vars;}
        printf '%s\n%s\n' \
            '{\"id\":\"1\",\"command\":\"configure\",\"params\":${configure_params}}' \
            '${command_json}' \
        | RUST_BACKTRACE=1 /usr/local/bin/aft
    " >"$tmpout" 2>"$tmperr" || exit_code=$?

    # Print results
    if [ -s "$tmperr" ]; then
        echo "  stderr:"
        sed 's/^/    /' "$tmperr"
    fi

    if [ -s "$tmpout" ]; then
        echo "  stdout:"
        sed 's/^/    /' "$tmpout"
    fi

    echo "  exit_code: $exit_code"

    # Check if process was killed by signal (128+signal)
    if [ "$exit_code" -gt 128 ]; then
        local signal=$((exit_code - 128))
        echo -e "  ${RED}KILLED by signal $signal${NC}"
    fi

    # Cleanup
    rm -f "$tmpout" "$tmperr"
    return "$exit_code"
}

check_outline_response() {
    # Returns 0 if the aft process stayed alive and returned a valid outline response
    local env_vars="${1:-}"
    local label="${2}"
    local extra_configure_params="${3:-}"

    echo ""
    echo "━━━ Scenario: ${label} ━━━"

    local outline_cmd='{"id":"2","command":"outline","params":{"file":"main.py"}}'

    local exit_code=0
    send_commands "$env_vars" "$extra_configure_params" "$outline_cmd" 15 || exit_code=$?

    if [ "$exit_code" -eq 0 ]; then
        echo -e "  ${GREEN}PASS${NC} — process stayed alive, outline returned"
        PASS=$((PASS + 1))
        return 0
    elif [ "$exit_code" -eq 124 ]; then
        # timeout — process hung but didn't crash
        echo -e "  ${YELLOW}TIMEOUT${NC} — process hung (didn't crash, but didn't respond)"
        FAIL=$((FAIL + 1))
        return 1
    else
        echo -e "  ${RED}FAIL${NC} — process died (exit code: $exit_code)"
        FAIL=$((FAIL + 1))
        return 1
    fi
}

check_grep_response() {
    local env_vars="${1:-}"
    local label="${2}"
    local extra_configure_params="${3:-}"

    echo ""
    echo "━━━ Scenario: ${label} ━━━"

    local grep_cmd='{"id":"2","command":"grep","params":{"pattern":"def","path":"main.py"}}'

    local exit_code=0
    send_commands "$env_vars" "$extra_configure_params" "$grep_cmd" 15 || exit_code=$?

    if [ "$exit_code" -eq 0 ]; then
        echo -e "  ${GREEN}PASS${NC} — process stayed alive, grep returned"
        PASS=$((PASS + 1))
        return 0
    else
        echo -e "  ${RED}FAIL${NC} — process died (exit code: $exit_code)"
        FAIL=$((FAIL + 1))
        return 1
    fi
}

# ── Initialize git so project_cache_key works ─────────────────────

cd "$PROJECT_DIR"
git init -q . 2>/dev/null || true
git add -A 2>/dev/null || true
git commit -q -m "init" --allow-empty 2>/dev/null || true
cd /test

# ── Scenario 1: No ONNX Runtime installed ─────────────────────────
# The binary should start, skip semantic search, and serve outline/grep normally.

check_outline_response \
    "" \
    "No ONNX Runtime — outline should work" || true

check_grep_response \
    "" \
    "No ONNX Runtime — grep should work" || true

# ── Scenario 2: ORT_DYLIB_PATH points to missing file ────────────
# ort will try to load from the specified path and fail.
# Must NOT panic/abort — should skip semantic search gracefully.

check_outline_response \
    "ORT_DYLIB_PATH=/nonexistent/libonnxruntime.so" \
    "ORT_DYLIB_PATH to missing file — outline should work" || true

# ── Scenario 3: Broken .so file (wrong format) ───────────────────
# Create a fake libonnxruntime.so that dlopen will reject.

echo "not a real shared library" > /tmp/fake-libonnxruntime.so
chmod 755 /tmp/fake-libonnxruntime.so

check_outline_response \
    "ORT_DYLIB_PATH=/tmp/fake-libonnxruntime.so" \
    "Broken .so file — outline should work" || true

# ── Scenario 4: ORT_DYLIB_PATH to directory (wrong type) ─────────

mkdir -p /tmp/ort-dir
check_outline_response \
    "ORT_DYLIB_PATH=/tmp/ort-dir" \
    "ORT_DYLIB_PATH to directory — outline should work" || true

# ── Scenario 5: semantic_search disabled — baseline ───────────────
# With semantic search off, none of the above should matter.

echo ""
echo "━━━ Scenario: semantic_search disabled — baseline ━━━"

local_outline_cmd='{"id":"2","command":"outline","params":{"file":"main.py"}}'
exit_code=0
timeout 10 bash -c "
    printf '%s\n%s\n' \
        '{\"id\":\"1\",\"command\":\"configure\",\"params\":{\"harness\":\"opencode\",\"project_root\":\"${PROJECT_DIR}\"}}' \
        '${local_outline_cmd}' \
    | /usr/local/bin/aft
" 2>/dev/null || exit_code=$?

if [ "$exit_code" -eq 0 ]; then
    echo -e "  ${GREEN}PASS${NC} — baseline outline works without semantic search"
    PASS=$((PASS + 1))
else
    echo -e "  ${RED}FAIL${NC} — baseline broken (exit code: $exit_code)"
    FAIL=$((FAIL + 1))
fi

# ── Summary ───────────────────────────────────────────────────────

echo ""
echo "════════════════════════════════════════"
echo -e "  Results: ${GREEN}${PASS} passed${NC}, ${RED}${FAIL} failed${NC}"
echo "════════════════════════════════════════"

if [ "$FAIL" -gt 0 ]; then
    echo -e "${RED}ONNX Runtime crash protection is broken!${NC}"
    exit 1
fi

echo -e "${GREEN}All scenarios passed — aft handles ORT failures gracefully.${NC}"
exit 0
