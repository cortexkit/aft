#!/usr/bin/env bash
# ------------------------------------------------------------------
# E2E test: AFT plugin running inside OpenCode.
#
# Used by both:
#   - tests/docker/Dockerfile.linux-x64 (Linux Docker E2E in CI)
#   - tests/macos-e2e/run.sh             (macOS native E2E in CI)
#
# Uses aimock for deterministic OpenAI-compatible mock LLM.
# Simulates a realistic multi-turn agent session that exercises:
#   - aft_outline, read, grep, glob, aft_search, edit, aft_safety
#   - Trigram search index (background build + query)
#   - Semantic search index (ONNX Runtime + fastembed)
#   - Multiple ONNX Runtime failure scenarios
#
# Each scenario runs a full OpenCode session with 8 tool call turns,
# giving background threads enough time to build indices.
#
# Platform-specific behavior is controlled by the AFT_E2E_PLATFORM env
# var (defaults to "linux"):
#   AFT_E2E_PLATFORM=linux  →  fake libonnxruntime.so in /usr/local/lib
#   AFT_E2E_PLATFORM=macos  →  fake libonnxruntime.dylib in /tmp
# Each platform's runner script is responsible for installing OpenCode,
# Bun, aimock, writing configs, and placing the AFT binary + plugin
# before invoking this script.
# ------------------------------------------------------------------

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

PASS=0
FAIL=0
PLATFORM="${AFT_E2E_PLATFORM:-linux}"
AIMOCK_RUN_DIR="${AFT_E2E_TEMP_ROOT:-${TMPDIR:-/tmp}/aimock-$$}"
mkdir -p "$AIMOCK_RUN_DIR"
export TMPDIR="$AIMOCK_RUN_DIR"
# The plugin writes its log to the durable AFT log directory (rotating sink),
# not $TMPDIR — mirror resolveAftLogPath(): AFT_CACHE_DIR/aft, else
# XDG_DATA_HOME, else ~/.local/share, + cortexkit/aft/logs/. The old
# $TMPDIR/aft-plugin.log default silently matched nothing once durable
# logging shipped, and the startup checks all grep this file.
if [ -n "${AFT_E2E_PLUGIN_LOG:-}" ]; then
    PLUGIN_LOG="$AFT_E2E_PLUGIN_LOG"
elif [ -n "${AFT_CACHE_DIR:-}" ]; then
    PLUGIN_LOG="$AFT_CACHE_DIR/aft/logs/aft-plugin.log"
else
    PLUGIN_LOG="${XDG_DATA_HOME:-$HOME/.local/share}/cortexkit/aft/logs/aft-plugin.log"
fi
# Start every run from a clean log so greps can't match a previous run's
# lines (the durable log persists across runs on non-container hosts).
mkdir -p "$(dirname "$PLUGIN_LOG")"
: > "$PLUGIN_LOG"
AIMOCK_PID=""

# Platform-specific paths for the broken-ONNX scenario.
case "$PLATFORM" in
    linux)
        FAKE_ORT_PATH="/usr/local/lib/libonnxruntime.so"
        PLATFORM_DISPLAY="Linux x64 (Debian)"
        ;;
    macos)
        # /tmp on macOS is a symlink to /private/tmp; we use a path the
        # plugin can find via DYLD_LIBRARY_PATH or the AFT-managed cache.
        # We don't drop into /usr/local/lib because SIP-protected paths
        # need root, and macOS GH Actions runners don't grant it.
        FAKE_ORT_PATH="${RUNNER_TEMP:-/tmp}/libonnxruntime.dylib"
        PLATFORM_DISPLAY="macOS native"
        ;;
    *)
        echo "Unknown AFT_E2E_PLATFORM: $PLATFORM (expected linux|macos)" >&2
        exit 2
        ;;
esac

check() {
    local label="$1"
    local condition="$2"
    if eval "$condition"; then
        echo -e "  ${GREEN}PASS${NC} [$label]"
        PASS=$((PASS + 1))
    else
        echo -e "  ${RED}FAIL${NC} [$label]"
        FAIL=$((FAIL + 1))
    fi
}

# Non-blocking check — logs warning but doesn't increment FAIL counter.
# Used for checks that fail under Docker QEMU emulation but pass on real Linux.
warn_check() {
    local label="$1"
    local condition="$2"
    if eval "$condition"; then
        echo -e "  ${GREEN}PASS${NC} [$label]"
        PASS=$((PASS + 1))
    else
        echo -e "  ${YELLOW}WARN${NC} [$label] (non-blocking — may fail under QEMU emulation)"
    fi
}

choose_aimock_port() {
    node <<'NODE'
const net = require("node:net");

let attempts = 0;
function probe() {
  if (++attempts > 100) {
    console.error("failed to find a free aimock port in 4000-9999");
    process.exit(1);
  }

  const port = 4000 + Math.floor(Math.random() * 6000);
  const server = net.createServer();
  server.unref();
  server.on("error", probe);
  server.listen(port, "127.0.0.1", () => {
    const selected = server.address().port;
    server.close(() => console.log(selected));
  });
}

probe();
NODE
}

configure_opencode_mock_port() {
    local config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/opencode"
    local config_file="$config_dir/opencode.json"
    if [ ! -f "$config_file" ]; then
        echo "OpenCode config not found: $config_file" >&2
        exit 2
    fi

    OPENCODE_CONFIG="$config_file" MOCK_BASE_URL="$AIMOCK_BASE_URL/v1" node <<'NODE'
const fs = require("node:fs");
const configPath = process.env.OPENCODE_CONFIG;
const baseURL = process.env.MOCK_BASE_URL;
const config = JSON.parse(fs.readFileSync(configPath, "utf8"));
config.provider = config.provider || {};
config.provider.mock = config.provider.mock || {};
config.provider.mock.options = config.provider.mock.options || {};
config.provider.mock.options.baseURL = baseURL;
fs.writeFileSync(configPath, JSON.stringify(config, null, 2) + "\n");
NODE
}

# AFT_E2E_MOCK_SERVER points to the mock-server.js entry. The Docker setup
# places it at /test/mock-server.js; the macOS runner places it relative to
# the repo checkout. Both wire this through env so we don't hardcode paths.
MOCK_SERVER="${AFT_E2E_MOCK_SERVER:-/test/mock-server.js}"
AIMOCK_PORT="${AFT_E2E_AIMOCK_PORT:-$(choose_aimock_port)}"
AIMOCK_BASE_URL="http://127.0.0.1:${AIMOCK_PORT}"
AIMOCK_LOG="$AIMOCK_RUN_DIR/aimock.log"

cleanup() {
    set +e
    if [ -n "${AIMOCK_PID:-}" ]; then
        kill "$AIMOCK_PID" 2>/dev/null || true
        wait "$AIMOCK_PID" 2>/dev/null || true
        AIMOCK_PID=""
    fi
    if [ -n "${FAKE_ORT_PATH:-}" ]; then
        rm -f "$FAKE_ORT_PATH"
    fi
    if [ -n "${AIMOCK_RUN_DIR:-}" ]; then
        rm -rf "$AIMOCK_RUN_DIR"
    fi
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

configure_opencode_mock_port

# Turn log: the mock appends a line each time it actually serves a turn
# fixture. The harness reads it to prove the agent loop progressed beyond the
# first request (tool result consumed → next request issued), which a hung or
# no-op session that merely hits the timeout cannot do.
TURN_LOG="$AIMOCK_RUN_DIR/turns.log"

start_aimock() {
    : > "$TURN_LOG"
    AIMOCK_PORT="$AIMOCK_PORT" AFT_E2E_TURN_LOG="$TURN_LOG" node "$MOCK_SERVER" > "$AIMOCK_LOG" 2>&1 &
    AIMOCK_PID=$!
    for i in $(seq 1 15); do
        if curl -s "$AIMOCK_BASE_URL/v1/models" > /dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    return 1
}

stop_aimock() {
    if [ -n "${AIMOCK_PID:-}" ]; then
        kill "$AIMOCK_PID" 2>/dev/null || true
        wait "$AIMOCK_PID" 2>/dev/null || true
        AIMOCK_PID=""
    fi
    # Wait for port to be freed
    sleep 2
}

run_opencode_session() {
    local prompt="$1"
    local result_file="$2"
    local timeout_secs="${3:-60}"

    set +e
    # OPENAI_API_KEY required for OpenCode's openai adapter to make requests.
    # `timeout` is only a safety bound; a healthy scripted session must exit 0.
    #
    # OPENCODE_DISABLE_DEFAULT_PLUGINS: on a cold cache, OpenCode's config load
    # forks an `npm install` of its own default plugin (@opencode-ai/plugin) into
    # .opencode/node_modules and plugin init joins that fiber under a 5-minute
    # lock before any external plugin loads (bootstrap.ts:34-38, config.ts:452-471,
    # plugin/index.ts:199, npm.ts:80-113). A slow registry turned that join into a
    # silent multi-minute stall with no log line and no provider request. AFT
    # does not need the host's default plugins, so skip the install.
    TMPDIR="$AIMOCK_RUN_DIR" \
    OPENAI_API_KEY=sk-mock-e2e-test \
    OPENCODE_DISABLE_DEFAULT_PLUGINS=true \
    timeout --signal=KILL "$timeout_secs" opencode run \
        --model "mock/mock-model" \
        "$prompt" \
        > "$result_file" 2>&1
    local exit_code=$?
    set -e

    if [ $exit_code -eq 124 ] || [ $exit_code -eq 137 ]; then
        echo "OpenCode timed out after ${timeout_secs}s (exit ${exit_code})" >&2
        # A stall before the first model request leaves the plugin log empty,
        # so OpenCode's own output is the only trace of where startup blocked.
        echo "  OpenCode output (last 40 lines of ${result_file}):" >&2
        if [ -s "$result_file" ]; then
            tail -n 40 "$result_file" | sed 's/^/    /' >&2
        else
            echo "    (empty)" >&2
        fi
        # With stdout/stderr empty, OpenCode's own log file is the only record
        # of what a stalled launch was doing (plugin install, provider fetches).
        local oc_log_dir="${XDG_DATA_HOME:-$HOME/.local/share}/opencode/log"
        local oc_log
        oc_log=$(ls -t "$oc_log_dir"/*.log 2>/dev/null | head -n 1 || true)
        if [ -n "$oc_log" ]; then
            echo "  OpenCode log (last 60 lines of ${oc_log}):" >&2
            tail -n 60 "$oc_log" | sed 's/^/    /' >&2
        else
            echo "  OpenCode log: none under ${oc_log_dir}" >&2
        fi
    fi
    return "$exit_code"
}

echo "════════════════════════════════════════"
echo "  AFT E2E Test — $PLATFORM_DISPLAY"
echo "════════════════════════════════════════"
echo ""

echo -n "OpenCode: "
opencode --version 2>&1 | head -1

echo -n "Node: "
node --version

echo "Run temp: $AIMOCK_RUN_DIR"
echo "aimock URL: $AIMOCK_BASE_URL/v1"
echo ""

# ══════════════════════════════════════════════════════════════════
# Scenario 1: Full realistic session — no ONNX on system
# Exercises: outline, read, grep, glob, aft_search, edit, undo
# Verifies: trigram index builds, semantic search degrades gracefully
# ══════════════════════════════════════════════════════════════════

echo "── Warm-up: first OpenCode launch (cold caches) ──"
echo ""

# The very first `opencode run` on a fresh image pays every cold-cache cost at
# once (plugin install from the file:// tarball, LSP server npm installs, the
# ONNX Runtime download) before it makes a single model request, and on a slow
# runner that alone can exceed a scenario budget while emitting nothing. Pay it
# here under its own generous budget so Scenario 1 measures the session, not
# the install. Success means only "OpenCode came back"; scenario assertions
# stay unchanged and the plugin log is reset before Scenario 1.
start_aimock
WARMUP_RESULT="$AIMOCK_RUN_DIR/result-warmup.txt"
if run_opencode_session "Say hello." "$WARMUP_RESULT" 240; then
    WARMUP_RC=0
else
    WARMUP_RC=$?
fi
echo "  warm-up exit code: $WARMUP_RC"
# 124/137 are the timeout wrapper's codes; any other non-zero exit still means
# OpenCode came back on its own, which is all the warm-up needs.
check "warm-up launch returned within 240s" "[ $WARMUP_RC -ne 124 ] && [ $WARMUP_RC -ne 137 ]"
# The warm-up is the only launch that sees a fresh cache, so its plugin log is
# the evidence for where a silent first start spends its time. Print it before
# Scenario 1 resets the log.
echo "  Plugin log after warm-up (last 40 lines):"
if [ -s "$PLUGIN_LOG" ]; then
    tail -n 40 "$PLUGIN_LOG" | sed 's/^/    /'
else
    echo "    (empty)"
fi
stop_aimock

echo "── Scenario 1: Full session (no ONNX Runtime) ──"
echo ""

rm -f "$PLUGIN_LOG"

echo "Starting aimock..."
start_aimock
check "aimock started" "curl -s '$AIMOCK_BASE_URL/v1/models' > /dev/null 2>&1"

echo "Running 8-turn OpenCode session..."
RESULT_FILE="$AIMOCK_RUN_DIR/result-scenario1.txt"
# 90s timeout (was default 30s — too tight): on a cold-cache run under
# QEMU emulation, ONNX Runtime download (~30MB) plus first-time npm
# install of 3 LSP servers (typescript-language-server, pyright,
# @biomejs/biome) routinely consume 25-40s before the first AFT tool
# call can even reach the bridge. With 30s, OpenCode hits the SIGKILL
# before any model interaction completes — the `Spawning binary` /
# `started, pid` log lines never appear because the bridge never
# actually got invoked. 90s gives realistic headroom for cold-cache
# runs while still bounding total runtime.
if run_opencode_session \
    "Explore this project: outline src, read main.py, grep for functions, glob for python files, search for greeting logic, edit main.py, then undo the edit." \
    "$RESULT_FILE" \
    90
then
    EXIT_CODE=0
else
    EXIT_CODE=$?
fi

echo "  OpenCode exit code: $EXIT_CODE"

# Basic health. exit 0 = clean. A timeout/SIGKILL is now a failure because
# OpenCode exits cleanly after the scripted session; the turn-progression check
# below remains the positive gate proving the agent loop actually ran multiple
# turns (consumed a tool result and issued the next request), which a hung/no-op
# session cannot fake.
check "clean OpenCode exit" "[ $EXIT_CODE -eq 0 ]"
check "no crash" "! grep -qi 'Binary crashed\|SIGABRT\|panicked' '$RESULT_FILE' 2>/dev/null"

# Agent loop actually progressed: the mock must have served at least the first
# few turns (outline → read → grep), which requires each tool call to round-trip
# through the bridge and return a usable result. This is the positive signal
# that replaces "session completed on timeout" false greens.
TURNS_SERVED=$(wc -l < "$TURN_LOG" 2>/dev/null | tr -d ' ')
TURN_LABELS=$(tr '\n' ' ' < "$TURN_LOG" 2>/dev/null)
echo "  Turns served by mock: ${TURNS_SERVED:-0} (${TURN_LABELS})"
check "agent loop completed all 8 scripted turns" "[ \"${TURNS_SERVED:-0}\" -eq 8 ]"
for turn_label in \
    turn-1-outline \
    turn-2-read \
    turn-3-grep \
    turn-4-glob \
    turn-5-aft_search \
    turn-6-edit \
    turn-7-undo \
    turn-8-final
do
    check "scripted tool turn served (${turn_label})" "grep -qx '$turn_label' '$TURN_LOG' 2>/dev/null"
done
check "unexpected mock fallback not used" "! grep -q 'unexpected-fallback' '$TURN_LOG' 2>/dev/null && ! grep -q 'UNEXPECTED_TURN_FALLBACK' '$RESULT_FILE' '$AIMOCK_LOG' 2>/dev/null"
check "edit was undone (main.py restored)" "grep -q 'print(greet(\"world\"))' src/main.py && ! grep -q 'print(greet(\"docker\"))' src/main.py"

# Plugin startup. Binary resolution must emit one operator-facing contract line
# for every successful path, naming both the source and final executable path.
check "plugin loaded" "grep -q 'Resolved binary from .*: ' '$PLUGIN_LOG' 2>/dev/null"
check "config loaded" "grep -q 'Config loaded' '$PLUGIN_LOG' 2>/dev/null"

# Bridge spawned and survived
check "bridge spawned" "grep -q 'Spawning binary\|started, pid' '$PLUGIN_LOG' 2>/dev/null"
check "no bridge crash" "! grep -qi 'Binary crashed\|SIGABRT' '$PLUGIN_LOG' 2>/dev/null"

# Search index
check "search index started" "grep -qi 'watcher started\|search.*index\|index.*build' '$PLUGIN_LOG' 2>/dev/null"

# ONNX — no crash is mandatory, download success is best-effort in Docker (QEMU limitations)
check "no ONNX crash" "! grep -qi 'SIGABRT\|panicked.*ort\|thread.*panicked' '$PLUGIN_LOG' 2>/dev/null"
# Note: ONNX download may fail under QEMU emulation due to curl/fetch timing issues.
# These are informational checks — not release-blocking.
if grep -qi 'ONNX Runtime.*installed to\|ONNX Runtime found at' "$PLUGIN_LOG" 2>/dev/null; then
    echo -e "  ${GREEN}PASS${NC} [ONNX downloaded]"
    PASS=$((PASS + 1))
    if grep -qi 'built semantic index\|semantic index persisted' "$PLUGIN_LOG" 2>/dev/null; then
        echo -e "  ${GREEN}PASS${NC} [semantic index built]"
        PASS=$((PASS + 1))
    else
        echo -e "  ${YELLOW}SKIP${NC} [semantic index built — ONNX downloaded but index not ready in time]"
    fi
else
    echo -e "  ${YELLOW}SKIP${NC} [ONNX downloaded — expected in Docker/QEMU, verify on real Linux]"
    echo -e "  ${YELLOW}SKIP${NC} [semantic index built — depends on ONNX download]"
fi

echo ""
echo "  Plugin log (last 30 lines):"
tail -30 "$PLUGIN_LOG" 2>/dev/null | sed 's/^/    /' || echo "    (empty)"

echo ""
echo "  aimock log:"
cat "$AIMOCK_LOG" 2>/dev/null | sed 's/^/    /' || echo "    (empty)"

stop_aimock

# ══════════════════════════════════════════════════════════════════
# Scenario 2: Broken ONNX Runtime library (reproduces issue #4)
# A fake shared library that the plugin detects and sets as
# ORT_DYLIB_PATH — the binary should NOT crash when loading it.
# Library suffix and path differ per platform; FAKE_ORT_PATH was
# resolved at the top of the script.
# ══════════════════════════════════════════════════════════════════

echo ""
echo "── Scenario 2: Broken ONNX Runtime library (issue #4) ──"
echo ""

# Install fake broken library at $FAKE_ORT_PATH (linux: /usr/local/lib/libonnxruntime.so,
# macos: $RUNNER_TEMP/libonnxruntime.dylib).
echo "not a real shared library" > "$FAKE_ORT_PATH"
chmod 755 "$FAKE_ORT_PATH"
echo "  Installed fake $(basename "$FAKE_ORT_PATH") at $FAKE_ORT_PATH"

rm -f "$PLUGIN_LOG"

start_aimock
check "aimock started (s2)" "curl -s '$AIMOCK_BASE_URL/v1/models' > /dev/null 2>&1"

echo "Running session with broken library..."
RESULT_FILE="$AIMOCK_RUN_DIR/result-scenario2.txt"
# On macOS, the AFT plugin probes a fixed list of system paths
# (/usr/local/lib, /opt/homebrew/lib) for libonnxruntime.dylib. Since
# we cannot write into /usr/local/lib on a vanilla GH Actions runner
# without sudo, we point ORT_DYLIB_PATH directly at our fake instead.
# Linux scenario keeps the implicit /usr/local/lib detection path.
if [ "$PLATFORM" = "macos" ]; then
    if ORT_DYLIB_PATH="$FAKE_ORT_PATH" \
        run_opencode_session \
            "Read the file src/main.py and then grep for all function definitions." \
            "$RESULT_FILE"
    then
        EXIT_CODE=0
    else
        EXIT_CODE=$?
    fi
else
    if run_opencode_session \
        "Read the file src/main.py and then grep for all function definitions." \
        "$RESULT_FILE"
    then
        EXIT_CODE=0
    else
        EXIT_CODE=$?
    fi
fi

echo "  OpenCode exit code: $EXIT_CODE"

check "clean exit (broken lib)" "[ $EXIT_CODE -eq 0 ]"
check "no crash (broken lib)" "! grep -qi 'Binary crashed\|SIGABRT\|panicked' '$RESULT_FILE' 2>/dev/null"
check "no plugin crash (broken lib)" "! grep -qi 'SIGABRT\|thread.*panicked' '$PLUGIN_LOG' 2>/dev/null"

# Verify the plugin detected the system library
warn_check "system ORT detected" "grep -q 'ONNX Runtime found at system path\|ORT_DYLIB_PATH' '$PLUGIN_LOG' 2>/dev/null"

echo ""
echo "  Plugin log (last 30 lines):"
tail -30 "$PLUGIN_LOG" 2>/dev/null | sed 's/^/    /' || echo "    (empty)"

rm -f "$FAKE_ORT_PATH"
stop_aimock

# ══════════════════════════════════════════════════════════════════
# Scenario 3: ORT_DYLIB_PATH to nonexistent file
# Tests that a bad env var doesn't crash the process.
# ══════════════════════════════════════════════════════════════════

echo ""
echo "── Scenario 3: ORT_DYLIB_PATH to missing file ──"
echo ""

rm -f "$PLUGIN_LOG"
if [ "$PLATFORM" = "macos" ]; then
    MISSING_ORT_PATH="${RUNNER_TEMP:-/tmp}/aft-test-ort-missing-$$.dylib"
else
    MISSING_ORT_PATH="/tmp/aft-test-ort-missing-$$.so"
fi
rm -f "$MISSING_ORT_PATH"

start_aimock
check "aimock started (s3)" "curl -s '$AIMOCK_BASE_URL/v1/models' > /dev/null 2>&1"

RESULT_FILE="$AIMOCK_RUN_DIR/result-scenario3.txt"
if ORT_DYLIB_PATH="$MISSING_ORT_PATH" \
    run_opencode_session \
        "Read src/main.py" \
        "$RESULT_FILE"
then
    EXIT_CODE=0
else
    EXIT_CODE=$?
fi

echo "  OpenCode exit code: $EXIT_CODE"

check "clean exit (missing ORT)" "[ $EXIT_CODE -eq 0 ]"
check "no crash (missing ORT)" "! grep -qi 'Binary crashed\|SIGABRT\|panicked' '$RESULT_FILE' 2>/dev/null"
# ORT_DYLIB_PATH propagation is covered structurally by scenario 2's
# "system ORT detected" assertion. The s3 path-grep is brittle because
# the plugin caches the semantic index across scenarios — when s3 finds
# a fresh cache from s1/s2 it never re-spawns the embedding pipeline and
# never logs the env-var path. Downgraded to warn_check; the unique
# s3 invariant is `no crash with missing path`, which IS gated.
warn_check "ORT_DYLIB_PATH passed (missing ORT)" "grep -Fq '$MISSING_ORT_PATH' '$PLUGIN_LOG' 2>/dev/null"
warn_check "semantic disabled gracefully (missing ORT)" "grep -qi 'failed to build semantic index.*ONNX Runtime not found\|Semantic search unavailable.*ONNX Runtime not found\|semantic_search_unavailable' '$PLUGIN_LOG' '$RESULT_FILE' 2>/dev/null"
check "other tools still work (missing ORT)" "grep -qi 'completed\|Task complete\|src/main.py' '$RESULT_FILE' 2>/dev/null"

stop_aimock

# ══════════════════════════════════════════════════════════════════
# Summary
# ══════════════════════════════════════════════════════════════════

echo ""
echo "════════════════════════════════════════"
echo -e "  Results: ${GREEN}${PASS} passed${NC}, ${RED}${FAIL} failed${NC}"
echo "════════════════════════════════════════"

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo -e "${RED}TESTS FAILED${NC}"
    exit 1
fi

echo -e "${GREEN}ALL TESTS PASSED${NC}"
exit 0
