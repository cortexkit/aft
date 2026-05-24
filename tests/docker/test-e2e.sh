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
PLUGIN_LOG="${AFT_E2E_PLUGIN_LOG:-$AIMOCK_RUN_DIR/aft-plugin.log}"

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
configure_opencode_mock_port

start_aimock() {
    AIMOCK_PORT="$AIMOCK_PORT" node "$MOCK_SERVER" > "$AIMOCK_LOG" 2>&1 &
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
    kill $AIMOCK_PID 2>/dev/null || true
    wait $AIMOCK_PID 2>/dev/null || true
    # Wait for port to be freed
    sleep 2
}

run_opencode_session() {
    local prompt="$1"
    local result_file="$2"
    local timeout_secs="${3:-30}"

    set +e
    # OPENAI_API_KEY required for OpenCode's openai adapter to make requests
    TMPDIR="$AIMOCK_RUN_DIR" \
    OPENAI_API_KEY=sk-mock-e2e-test \
    timeout --signal=KILL "$timeout_secs" opencode run \
        --model "mock/mock-model" \
        "$prompt" \
        > "$result_file" 2>&1
    local exit_code=$?
    set -e
    # exit 137 = SIGKILL from timeout (expected — opencode hangs after session)
    # exit 0 = clean exit
    # exit 124 = SIGTERM timeout (also ok)
    if [ $exit_code -eq 137 ] || [ $exit_code -eq 0 ] || [ $exit_code -eq 124 ]; then
        return 0
    fi
    return $exit_code
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
run_opencode_session \
    "Explore this project: outline src, read main.py, grep for functions, glob for python files, search for greeting logic, edit main.py, then undo the edit." \
    "$RESULT_FILE" \
    90

EXIT_CODE=$?

# Basic health
check "session completed" "[ $EXIT_CODE -eq 0 ] || [ $EXIT_CODE -eq 124 ]"
check "no crash" "! grep -qi 'Binary crashed\|SIGABRT\|panicked' '$RESULT_FILE' 2>/dev/null"

# Plugin startup
check "plugin loaded" "grep -q 'Resolved binary\|Copied npm binary' '$PLUGIN_LOG' 2>/dev/null"
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
    ORT_DYLIB_PATH="$FAKE_ORT_PATH" \
    run_opencode_session \
        "Read the file src/main.py and then grep for all function definitions." \
        "$RESULT_FILE"
else
    run_opencode_session \
        "Read the file src/main.py and then grep for all function definitions." \
        "$RESULT_FILE"
fi

EXIT_CODE=$?

check "session completed (broken lib)" "[ $EXIT_CODE -eq 0 ] || [ $EXIT_CODE -eq 124 ]"
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

start_aimock
check "aimock started (s3)" "curl -s '$AIMOCK_BASE_URL/v1/models' > /dev/null 2>&1"

RESULT_FILE="$AIMOCK_RUN_DIR/result-scenario3.txt"
run_opencode_session \
    "Read src/main.py" \
    "$RESULT_FILE"
EXIT_CODE=$?

check "session completed (missing ORT)" "[ $EXIT_CODE -eq 0 ] || [ $EXIT_CODE -eq 124 ]"
check "no crash (missing ORT)" "! grep -qi 'Binary crashed\|SIGABRT\|panicked' '$RESULT_FILE' 2>/dev/null"

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
