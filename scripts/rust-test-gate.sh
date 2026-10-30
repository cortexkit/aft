#!/usr/bin/env bash
set -euo pipefail

# An ambient AFT_STORAGE_DIR (e.g. injected into agent bash lanes) outranks
# the XDG isolation test fixtures rely on and silently points the suite at the
# real shared store. Gates always run with default resolution.
unset AFT_STORAGE_DIR

# Names this process tree as the Rust test gate. It is exported apart from the
# home isolation below on purpose: the in-suite hermeticity check
# (`gate_resolves_every_user_config_and_state_path_under_the_gate_homes`)
# treats "inside the gate but the isolation marker is missing" as a failure,
# so deleting the isolation block must not also delete this signal.
export AFT_RUST_TEST_GATE=1

# Run the entire gate at reduced scheduling priority so saturated test
# windows cannot starve the supervised ck-* modules into missing health
# probes (three health-kills on 2026-08-08 traced to gate-window load).
# taskpolicy demotes to the utility QoS class on macOS (E-cores under
# contention); nice covers Linux and the priority dimension everywhere.
# Self-demotion only covers load WE generate; the supervisor-side
# threshold fix covers foreign load.
if [ -z "${AFT_GATE_DEMOTED:-}" ]; then
  export AFT_GATE_DEMOTED=1
  # nice-only, deliberately NOT taskpolicy utility: the QoS class pins the
  # whole gate to E-cores, and the integration suite's per-test 60s response
  # deadlines then fail wholesale (three gate runs red while the same suite
  # was green undemoted). nice yields to the normal-priority ck-* modules
  # under contention but keeps P-cores when the machine has headroom, which
  # is the property the demotion exists for.
  exec nice -n 10 "$0" "$@"
fi

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

# A test that exercises the plugin-less storage fallback must never inherit the
# operator's real data root. Keep the whole gate hermetic, including test binaries
# that launch child AFT processes without the shared integration helper.
unset AFT_CACHE_DIR
# Every XDG base directory gets a per-gate home, not just the data home: the
# `aft` processes the tests spawn otherwise read the operator's user config
# (`$XDG_CONFIG_HOME/cortexkit/aft.jsonc`, which can carry connection files,
# embedding endpoints, format and LSP settings) and read and rewrite the live
# gh-shim state (`$XDG_STATE_HOME/cortexkit/aft/gh-shim/rung-cache.json`).
# HOME itself is deliberately left alone: cargo, rustup and nextest resolve
# their toolchains from it, and git resolves the committer identity the
# fixture repos need from `~/.gitconfig`. AFT's own config, state, cache and
# storage resolvers consult these XDG variables before falling back to HOME.
gate_home_root="${AFT_GATE_HOME_ROOT:-${CARGO_TARGET_DIR:-$PWD/target}/aft-gate-home}"
# Git Bash on Windows spells paths as /d/a/...; native Windows code does not
# treat that as absolute, and several resolvers skip non-absolute XDG values.
# Hand the test processes the drive-letter form instead.
if command -v cygpath >/dev/null 2>&1; then
  gate_home_root="$(cygpath -m "$gate_home_root")"
fi
export XDG_DATA_HOME="$gate_home_root/data"
export XDG_CONFIG_HOME="$gate_home_root/config"
export XDG_STATE_HOME="$gate_home_root/state"
export XDG_CACHE_HOME="$gate_home_root/cache"
mkdir -p "$XDG_DATA_HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME"
# On Windows the per-user AFT cache (the shared managed git hooks, among
# others) follows %LOCALAPPDATA% rather than XDG_CACHE_HOME, so isolate it too.
if command -v cygpath >/dev/null 2>&1; then
  export LOCALAPPDATA="$gate_home_root/localappdata"
  mkdir -p "$LOCALAPPDATA"
fi
# The in-suite hermeticity check asserts every resolved config, state, cache
# and storage path lies under this root.
export AFT_GATE_HERMETIC_HOME_ROOT="$gate_home_root"

# aft's agent shells inject command-scope git config (core.hooksPath pointing
# at the live managed hook dispatchers) and a co-author trailer. A gate started
# from such a shell would pass both to every fixture `git` a test runs, so the
# operator's live hooks would run against test repos and fixture commits would
# carry the trailer. Drop the whole injected block, like AFT_STORAGE_DIR above.
unset GIT_CONFIG_COUNT GIT_CONFIG_PARAMETERS AFT_GIT_CO_AUTHOR
while IFS= read -r injected_git_config_var; do
  unset "$injected_git_config_var"
done < <(compgen -e | grep -E '^GIT_CONFIG_(KEY|VALUE)_[0-9]+$' || true)

runner="${AFT_RUST_TEST_RUNNER:-nextest}"
unit_runner="${AFT_UNIT_TEST_RUNNER:-cargo}"

if [[ "${AFT_CAPTURE_CRASH_DIAGNOSTICS:-}" == "1" ]]; then
  # Limits are process-local, so this must run in the same shell that later
  # launches nextest. The integration harness also prints SIGBUS/SIGSEGV fault
  # addresses and stacks; a core remains the fallback if unwinding is damaged.
  ulimit -c unlimited || echo "warning: unable to enable core dumps" >&2
  case "$(uname)" in
    Linux)
      printf 'core dump pattern: '
      cat /proc/sys/kernel/core_pattern 2>/dev/null || echo '<unavailable>'
      ;;
    Darwin)
      printf 'core dump pattern: '
      sysctl -n kern.corefile 2>/dev/null || echo '<unavailable>'
      ;;
  esac
fi

# Shared-box gate mutual exclusion. Full serial gates saturate this machine
# badly enough to fail NEIGHBORING seats' work (spawn timeouts in release
# pipelines, cross-gate test flakes, GC races) - three separate casualties on
# 2026-08-25 alone. Convention agreed across seats: full gates take a
# machine-wide advisory lock and wait for each other; focused runs (explicit
# AFT_GATE_PHASES subsets) skip it because they do not saturate the box.
# The lock is check-and-wait with a 2h staleness floor keyed on started_at,
# so a crashed gate never wedges the box. AFT_SKIP_BOX_GATE=1 bypasses (CI
# runners are not shared boxes; the lock is for the dev machine).
box_gate_lock="${AFT_BOX_GATE_LOCK:-$HOME/.local/share/cortexkit/box-gate.lock}"
box_gate_acquired=""
acquire_box_gate() {
  [ -n "${AFT_SKIP_BOX_GATE:-}" ] && return 0
  [ -n "${CI:-}" ] && return 0
  [ -n "${AFT_GATE_PHASES:-}" ] && return 0
  local waited=0
  while :; do
    if [ -f "$box_gate_lock" ]; then
      local started
      # started_at is epoch seconds by convention, but other seats have
      # written ISO-8601 strings (MC, 2026-08-30 incident: bash arithmetic on
      # "2026-08..." dies on octal "08"). Coerce both forms to epoch here and
      # print 0 for anything unparseable so the staleness floor treats a
      # malformed lock as stale instead of crashing the gate.
      started=$(python3 -c "
import json, datetime
try:
    raw = json.load(open('$box_gate_lock')).get('started_at', 0)
    if isinstance(raw, str):
        raw = datetime.datetime.fromisoformat(raw.replace('Z', '+00:00')).timestamp()
    print(int(raw))
except Exception:
    print(0)
" 2>/dev/null || echo 0)
      local now age
      now=$(date +%s)
      age=$((now - started))
      if [ "$age" -lt 7200 ]; then
        if [ "$waited" -eq 0 ]; then
          echo "==> box-gate: waiting for $(cat "$box_gate_lock" 2>/dev/null | head -c 200)"
        fi
        sleep 30
        waited=$((waited + 30))
        # Give up after 90 minutes of waiting and proceed: a full gate rarely
        # exceeds that, and refusing forever would strand pushes behind a
        # neighbor's marathon. The proceed is loud, never silent.
        if [ "$waited" -ge 5400 ]; then
          echo "==> box-gate: waited ${waited}s; proceeding alongside the holder (loud overlap)"
          return 0
        fi
        continue
      fi
      echo "==> box-gate: breaking stale lock (${age}s old)"
      rm -f "$box_gate_lock"
    fi
    mkdir -p "$(dirname "$box_gate_lock")"
    printf '{"seat":"AFT","task_id":"gate-%s-%s","started_at":%s}' "$$" "$(hostname -s 2>/dev/null || echo box)" "$(date +%s)" > "$box_gate_lock"
    box_gate_acquired=1
    trap 'release_box_gate' EXIT
    return 0
  done
}
release_box_gate() {
  [ -n "$box_gate_acquired" ] && rm -f "$box_gate_lock"
}
acquire_box_gate

# Keep every phase's output in a durable location. The default is deliberately
# outside /tmp because macOS cleanup can remove a still-running gate's only
# failure record. It is also outside CortexKit's data and state folders: phase
# commands write straight into this file, so it is every test's inherited
# stdout, and the native sandbox refuses a test whose stdout lies under a
# folder its floor denies. It is based on HOME, not XDG_STATE_HOME, because
# the gate points XDG_STATE_HOME into its own per-gate home under target/,
# which a run can replace while the gate is still writing its log.
gate_log_dir="${AFT_GATE_LOG_DIR:-$HOME/.local/state/aft-gate-logs}"
gate_log="${AFT_GATE_LOG:-$gate_log_dir/gate-$(date -u '+%Y%m%dT%H%M%SZ')-$$.log}"
mkdir -p "$(dirname "$gate_log")"
if [[ "${AFT_GATE_LOG_INHERITED:-}" == "1" ]]; then
  printf '\n--- nested rust test gate ---\n' >> "$gate_log"
else
  : > "$gate_log"
fi
export AFT_GATE_LOG="$gate_log"
export AFT_GATE_LOG_INHERITED=1

announce_gate_line() {
  local line="$1"
  printf '%s\n' "$line"
  printf '%s\n' "$line" >> "$gate_log"
}

announce_gate_line "gate log: $gate_log"

# An error that ends the gate before or between phases goes to stderr and into
# the gate log, so a downloaded log still says why the job failed.
gate_error() {
  printf '%s\n' "$1" >&2
  printf '%s\n' "$1" >> "$gate_log"
}

failed_test_phases=0
first_test_failure=0

# CI needs one complete failure inventory per runner allocation. Keep local
# fail-fast behavior and every existing nextest profile/timeout unchanged.
gate_cargo() {
  if [[ -n "${CI:-}" && "${1:-}" == "nextest" && "${2:-}" == "run" ]]; then
    shift 2
    cargo nextest run --no-fail-fast "$@"
  elif [[ -n "${CI:-}" && "${1:-}" == "test" ]]; then
    shift
    cargo test --no-fail-fast "$@"
  else
    cargo "$@"
  fi
}

finish_gate() {
  if [[ "$failed_test_phases" -ne 0 ]]; then
    announce_gate_line "GATE FAILED ($failed_test_phases test phases failed)"
    return "$first_test_failure"
  fi
  announce_gate_line "GATE PASSED"
}

report_gate_failure() {
  local label="$1"
  local rc="$2"
  local failed_names

  failed_names="$(grep -F 'FAIL [' "$gate_log" 2>/dev/null || true)"
  {
    printf '==> gate phase FAILED: %s (rc=%s)\n' "$label" "$rc"
    printf '==> failing test names (nextest FAIL [ summary)\n'
    if [[ -n "$failed_names" ]]; then
      printf '%s\n' "$failed_names"
    else
      printf '%s\n' '(no nextest FAIL [ summary was emitted)'
    fi
  } >> "$gate_log"

  # Append the failing test names before taking the tail, so fail-fast output still
  # identifies the test that caused the failure even when nextest stops before
  # producing a complete report.
  tail -n 60 "$gate_log" >&2 || true
}

# Copy everything appended to the gate log after byte OFFSET to stdout, once a
# second, until STOP_FILE exists (then one final copy) or the gate shell that
# started it is gone. Runs in the background beside a phase command.
stream_gate_log() {
  local offset="$1"
  local stop_file="$2"
  local gate_pid="$3"
  local size stop
  while :; do
    stop=0
    if [[ -e "$stop_file" ]] || ! kill -0 "$gate_pid" 2>/dev/null; then
      stop=1
    fi
    size=$(($(wc -c < "$gate_log")))
    if (( size > offset )); then
      tail -c "+$((offset + 1))" "$gate_log" | head -c "$((size - offset))"
      offset=$size
    fi
    (( stop )) && return 0
    sleep 1
  done
}

run_phase() {
  local label="$1"
  shift
  local started=$SECONDS
  local rc
  local offset stop_dir streamer_pid

  # Also proves the log is writable: under `set -e` a log that cannot be
  # written ends the gate here, before any phase command runs.
  announce_gate_line "==> $label"
  # The phase command writes straight into the gate log file and a background
  # loop copies the new bytes to the console. It is deliberately not piped
  # through `tee`: on Windows every process a test leaks inherits the write
  # end of any pipe above it, so a leaked grandchild kept `tee` from seeing
  # end-of-file and held a CI step open long after nextest had printed its
  # summary and exited. A file has no reader waiting for end-of-file, so a
  # leaked process holding it delays nothing.
  offset=$(($(wc -c < "$gate_log")))
  stop_dir="$(mktemp -d "${TMPDIR:-/tmp}/aft-gate-phase.XXXXXX")"
  # errexit is off before the streamer forks: its `tail | head` pipeline ends
  # with SIGPIPE by design and must not stop the copy loop.
  set +e
  stream_gate_log "$offset" "$stop_dir/done" "$$" &
  streamer_pid=$!
  # `exec` replaces the inherited console descriptors instead of saving
  # copies of them, so the command and its children never hold the console
  # pipe.
  (
    exec >>"$gate_log" 2>&1
    "$@"
  )
  rc=$?
  : > "$stop_dir/done"
  wait "$streamer_pid"
  rm -rf "$stop_dir"
  set -e
  if [[ "$rc" -ne 0 ]]; then
    report_gate_failure "$label" "$rc"
    # Only independent test commands may continue after a red phase. Failed
    # archive extraction, signing or lint are fatal prerequisites.
    if [[ -n "${CI:-}" && "${1:-}" == "gate_cargo" \
      && ( "${2:-}" == "test" || ( "${2:-}" == "nextest" && "${3:-}" == "run" ) ) ]]; then
      failed_test_phases=$((failed_test_phases + 1))
      if [[ "$first_test_failure" -eq 0 ]]; then
        first_test_failure="$rc"
      fi
      return 0
    fi
    announce_gate_line "GATE FAILED"
    return "$rc"
  fi

  announce_gate_line "    ok ($((SECONDS - started))s)"
}

# Gatekeeper assesses newly linked Mach-O executables that carry provenance.
# The assessment invokes XProtect and can show a focus-stealing verification UI.
# Sign with a stable local identifier for diagnosable logs, then execute once here
# so timed test processes do not all trigger the same expensive first execution.
# A fresh inode is assessed even when its signed bytes and cdhash are identical,
# so every worktree's freshly built binary still needs this controlled warm-up.
warm_macos_executable() {
  local binary="$1"
  shift

  [[ -x "$binary" ]] || return 0
  # Do not rewrite an already-stably-signed inode: replacing it would make
  # Gatekeeper assess the same product again instead of reusing the warm result.
  if ! codesign -d -vv "$binary" 2>&1 | grep -Fq 'Identifier=aft-dev-gate'; then
    codesign -f -s - --identifier aft-dev-gate "$binary" >/dev/null 2>&1 || true
  fi
  "$binary" "$@" >/dev/null 2>&1 || true
}

warm_macos_test_binaries() {
  # Ask cargo for the exact test-harness executables it built (the `executable`
  # field in the build JSON — not the incremental fragments under deps/). $@ =
  # the cargo build arguments that define the profile and scope.
  local bins
  bins="$(cargo test "$@" --no-run --message-format=json 2>/dev/null | python3 -c "
import sys, json
seen = set()
for line in sys.stdin:
    try: o = json.loads(line)
    except Exception: continue
    e = o.get('executable')
    if e: seen.add(e)
for p in sorted(seen): print(p)
")"
  local bin
  while IFS= read -r bin; do
    [[ -n "$bin" && -x "$bin" ]] || continue
    warm_macos_executable "$bin" --list
  done <<< "$bins"

  # The CLI is spawned by integration tests but is not a test harness, so it
  # does not appear in cargo's `executable` build messages.
  local target_dir="${CARGO_TARGET_DIR:-target}"
  local aft_binary="$target_dir/debug/aft"
  warm_macos_executable "$aft_binary" --version
}

if [[ "$runner" != "nextest" && "$runner" != "cargo" ]]; then
  gate_error "Unsupported AFT_RUST_TEST_RUNNER='$runner' (expected 'nextest' or 'cargo')"
  exit 2
fi
if [[ "$unit_runner" != "cargo" && "$unit_runner" != "nextest" ]]; then
  gate_error "Unsupported AFT_UNIT_TEST_RUNNER='$unit_runner' (expected 'cargo' or 'nextest')"
  exit 2
fi

# Keep focused phase selections byte-for-byte compatible: lint is part of the
# default all-phases gate, while an explicit subset remains responsible only for
# the phases it names (CI already runs the workspace lint separately).
if [[ "${AFT_GATE_PHASES:-all}" == "all" ]]; then
  run_phase "bunx biome check ." bunx biome check .
fi

if [[ "$runner" == "cargo" ]]; then
  if [[ "$(uname)" == "Darwin" && "${AFT_GATE_NO_XPROTECT_REMEDIATION:-}" != "1" ]]; then
    run_phase "warm macOS Gatekeeper assessment: sign + exec debug test binaries and target/debug/aft" \
      warm_macos_test_binaries --workspace
  fi
  run_phase "cargo test --workspace --quiet" gate_cargo test --workspace --quiet
  finish_gate
  exit 0
fi

if ! command -v cargo-nextest >/dev/null 2>&1; then
  gate_error "cargo-nextest is required; install it with: cargo install cargo-nextest --locked"
  exit 127
fi

extract_nextest_archive_for_prewarm() {
  local archive_file="${1:-${AFT_NEXTEST_ARCHIVE_FILE:-}}"
  local extract_to="${2:-$AFT_NEXTEST_EXTRACT_TO}"
  if [[ -z "$archive_file" ]]; then
    gate_error "AFT_NEXTEST_ARCHIVE_FILE is required to extract a nextest archive"
    return 2
  fi

  # Newer cargo-nextest (>= ~0.9.13x) refuses --extract-to when the
  # destination already holds a target/ tree (older versions merged into
  # it). CI restores a cached target/ into the workspace before this step
  # runs, so clear the collision: the archive fully replaces that tree.
  mkdir -p "$extract_to"
  df -h "$extract_to"
  if [ -d "$extract_to/target" ]; then
    rm -rf "$extract_to/target"
  fi
  cargo nextest list \
    --archive-file "$archive_file" \
    --extract-to "$extract_to" \
    --workspace-remap "$PWD" \
    --list-type binaries-only \
    --message-format json \
    --no-pager >/dev/null
}

# `cargo test --workspace -- --list` currently reports zero doctests for both
# workspace crates (`aft` and `aft_tokenizer`), so the split gate omits
# `cargo test --workspace --doc` until doctests actually exist.
#
# A CI lane can set AFT_UNIT_TEST_RUNNER=nextest so a nonterminating unit test
# is named and bounded by the gate profile instead of leaving one silent libtest
# process until the surrounding job times out.
#
# CI can split the independent execution phases after one job creates a nextest
# archive. The default remains `all`, preserving the single-machine gate used
# locally and by workflows that do not opt into sharding.
requested_phases="${AFT_GATE_PHASES:-all}"
if [[ "$requested_phases" == "all" ]]; then
  # `lib-shard` is opt-in; the ordinary all-phases gate remains unpartitioned.
  phase_enabled() { [[ "$1" != "lib-shard" ]]; }
else
  IFS=',' read -r -a selected_phases <<< "$requested_phases"
  for selected_phase in "${selected_phases[@]}"; do
    case "$selected_phase" in
      lib|lib-shard|nextest|watcher|conformance|storm) ;;
      *)
        gate_error "Unsupported AFT_GATE_PHASES entry '$selected_phase' (expected lib, lib-shard, nextest, watcher, conformance, storm, or all)"
        exit 2
        ;;
    esac
  done

  phase_enabled() {
    local wanted="$1"
    local selected_phase
    for selected_phase in "${selected_phases[@]}"; do
      [[ "$selected_phase" == "$wanted" ]] && return 0
    done
    return 1
  }
fi

lib_archive_args=(--archive-file "${AFT_NEXTEST_ARCHIVE_FILE:-}")
bin_archive_args=(--archive-file "${AFT_NEXTEST_ARCHIVE_BIN_FILE:-}")
if phase_enabled lib-shard \
  && [[ "$(uname)" == "Darwin" \
    && -n "${AFT_NEXTEST_ARCHIVE_FILE:-}" \
    && -n "${AFT_NEXTEST_EXTRACT_TO:-}" ]]; then
  # The isolated keychain probe runs before the libtest partition. Extract and
  # prewarm its fresh archive binaries first so Gatekeeper does not interrupt
  # a timed test with its first-execution assessment.
  mkdir -p "$AFT_NEXTEST_EXTRACT_TO"
  libtest_archives=("$AFT_NEXTEST_ARCHIVE_FILE")
  libtest_roots=("$AFT_NEXTEST_EXTRACT_TO")
  if [[ -n "${AFT_NEXTEST_ARCHIVE_BIN_FILE:-}" ]]; then
    libtest_archives+=("$AFT_NEXTEST_ARCHIVE_BIN_FILE")
    # Keep the bin archive below the replaced target tree, but in its own root:
    # extracting it must not delete or overwrite the prewarmed library binaries.
    libtest_roots+=("$AFT_NEXTEST_EXTRACT_TO/target/nextest-bin")
  fi
  for index in "${!libtest_archives[@]}"; do
    extract_to="${libtest_roots[$index]}"
    run_phase "extract nextest archive for macOS libtest prewarming" \
      extract_nextest_archive_for_prewarm "${libtest_archives[$index]}" "$extract_to"
    run_phase "warm macOS extracted libtest binaries" \
      env AFT_NEXTEST_EXTRACT_TO="$extract_to" ./scripts/warm-macos-nextest-archive.sh
    # Reuse these exact inodes. --archive-file would extract another copy and
    # bypass both prewarming and its disk-space measurement.
    extracted_args=(
      --cargo-metadata "$extract_to/target/nextest/cargo-metadata.json"
      --binaries-metadata "$extract_to/target/nextest/binaries-metadata.json"
      --target-dir-remap "$extract_to/target"
      --workspace-remap "$PWD"
    )
    if [[ "$index" == 0 ]]; then
      lib_archive_args=("${extracted_args[@]}")
    else
      bin_archive_args=("${extracted_args[@]}")
    fi
  done
fi

if phase_enabled lib; then
  # The platform-verifier TLS test spawns a subprocess whose keychain trust
  # evaluation is unbounded on Macs with third-party root CAs (NordVPN: ~10s
  # quiet, minutes under full-suite load — blew a 600s budget twice on
  # 2026-08-09). Isolation is the fix, not a bigger budget: run it alone
  # first (seconds when serial), then exclude it from the parallel phase.
  if [[ -n "${AFT_NEXTEST_ARCHIVE_FILE:-}" ]]; then
    run_phase "cargo nextest run from library archive inventory -E kind(lib) platform_verifier_tls_client_subprocess (serial: keychain-latency-sensitive)" \
      gate_cargo nextest run "${lib_archive_args[@]}" --profile gate \
        -E 'kind(lib)' -- platform_verifier_tls_client_subprocess
  else
    run_phase "cargo test -p agent-file-tools --lib platform_verifier_tls_client_subprocess --quiet (serial: keychain-latency-sensitive)" \
      gate_cargo test -p agent-file-tools --lib platform_verifier_tls_client_subprocess --quiet
  fi

  if phase_enabled lib-shard \
    && [[ -n "${AFT_NEXTEST_ARCHIVE_FILE:-}" && "$(uname)" != "Darwin" ]]; then
    # Windows needs its local PDB for the child backtrace; the macOS archive job
    # runs this probe before upload while its linked debug objects are present.
    run_phase "cargo test -p agent-file-tools --lib panic_hook_preserves_default_backtrace_behavior_and_persists --quiet (serial: child backtrace needs the local PDB)" \
      gate_cargo test -p agent-file-tools --lib panic_hook_preserves_default_backtrace_behavior_and_persists --quiet
  fi

  # When lib-shard is selected, the archive-backed command below owns the rest
  # of the unit inventory. Other callers keep the original whole-suite command.
  if ! phase_enabled lib-shard; then
    if [[ "$unit_runner" == "nextest" ]]; then
      run_phase "cargo nextest run --workspace --lib --bins --profile gate" \
        gate_cargo nextest run --workspace --lib --bins --profile gate -- \
          --skip platform_verifier_tls_client_subprocess
    else
      run_phase "cargo test --workspace --lib --bins --quiet" \
        gate_cargo test --workspace --lib --bins --quiet -- \
          --skip platform_verifier_tls_client_subprocess
    fi
  fi
fi

if phase_enabled lib-shard; then
  if [[ "$unit_runner" != "nextest" ]]; then
    gate_error "The lib-shard phase requires AFT_UNIT_TEST_RUNNER=nextest"
    exit 2
  fi
  if [[ -z "${AFT_NEXTEST_ARCHIVE_FILE:-}" || -z "${AFT_NEXTEST_PARTITION:-}" ]]; then
    gate_error "The lib-shard phase requires AFT_NEXTEST_ARCHIVE_FILE and AFT_NEXTEST_PARTITION"
    exit 2
  fi
  if [[ -n "${AFT_NEXTEST_ARCHIVE_BIN_FILE:-}" ]]; then
    run_phase "cargo nextest run from library archive inventory -E kind(lib) --partition $AFT_NEXTEST_PARTITION --profile gate" \
      gate_cargo nextest run "${lib_archive_args[@]}" --profile gate \
        -E 'kind(lib)' \
        --partition "$AFT_NEXTEST_PARTITION" \
        -- \
          --skip platform_verifier_tls_client_subprocess \
          --skip panic_hook_preserves_default_backtrace_behavior_and_persists
    run_phase "cargo nextest run from binary archive inventory -E kind(bin) --partition $AFT_NEXTEST_PARTITION --profile gate" \
      gate_cargo nextest run "${bin_archive_args[@]}" --profile gate \
        -E 'kind(bin)' \
        --partition "$AFT_NEXTEST_PARTITION"
  else
    run_phase "cargo nextest run from unit archive inventory -E 'kind(lib) | kind(bin)' --partition $AFT_NEXTEST_PARTITION --profile gate" \
      gate_cargo nextest run "${lib_archive_args[@]}" --profile gate \
        -E 'kind(lib) | kind(bin)' \
        --partition "$AFT_NEXTEST_PARTITION" \
        -- \
          --skip platform_verifier_tls_client_subprocess \
          --skip panic_hook_preserves_default_backtrace_behavior_and_persists
  fi
fi

if phase_enabled nextest && [[ "$(uname)" == "Darwin" && -z "${AFT_NEXTEST_ARCHIVE_FILE:-}" && "${AFT_GATE_NO_XPROTECT_REMEDIATION:-}" != "1" ]]; then
  run_phase "warm macOS Gatekeeper assessment: sign + exec debug test binaries and target/debug/aft" \
    bash -c "$(declare -f warm_macos_executable)
      $(declare -f warm_macos_test_binaries)
      warm_macos_test_binaries --workspace"
fi

if phase_enabled nextest; then
  # The gate profile disables retries by default; only the specific test overrides
  # listed in .config/nextest.toml may retry. Do not replace this with a blanket
  # retry profile.
  nextest_args=(gate_cargo nextest run --profile gate)
  if [[ -n "${AFT_NEXTEST_ARCHIVE_FILE:-}" && -n "${AFT_NEXTEST_EXTRACT_TO:-}" ]]; then
    # macOS archive shards need a stable extraction path so fresh test-binary
    # inodes can be signed and executed before nextest starts timed tests.
    # `binaries-only` extracts metadata without executing the binaries first.
    mkdir -p "$AFT_NEXTEST_EXTRACT_TO"
    run_phase "extract nextest archive for prewarming" \
      extract_nextest_archive_for_prewarm
    run_phase "warm macOS extracted test binaries" \
      ./scripts/warm-macos-nextest-archive.sh
    nextest_label="cargo nextest run from extracted archive metadata -E kind(test) - binary(=watcher_integration)"
    nextest_args+=(
      --cargo-metadata "$AFT_NEXTEST_EXTRACT_TO/target/nextest/cargo-metadata.json"
      --binaries-metadata "$AFT_NEXTEST_EXTRACT_TO/target/nextest/binaries-metadata.json"
      --target-dir-remap "$AFT_NEXTEST_EXTRACT_TO/target"
      --workspace-remap "$PWD"
    )
  elif [[ -n "${AFT_NEXTEST_ARCHIVE_FILE:-}" ]]; then
    nextest_label="cargo nextest run --archive-file $AFT_NEXTEST_ARCHIVE_FILE -E kind(test) - binary(=watcher_integration)"
    nextest_args+=(--archive-file "$AFT_NEXTEST_ARCHIVE_FILE")
  else
    nextest_label="cargo nextest run --workspace -E kind(test) - binary(=watcher_integration)"
    nextest_args+=(--workspace)
  fi
  nextest_args+=(-E 'kind(test) - binary(=watcher_integration)')
  if [[ -n "${AFT_NEXTEST_PARTITION:-}" ]]; then
    nextest_label+=" --partition $AFT_NEXTEST_PARTITION"
    nextest_args+=(--partition "$AFT_NEXTEST_PARTITION")
  fi
  run_phase "$nextest_label --profile gate" "${nextest_args[@]}"
fi

if phase_enabled watcher; then
  # test-timing-hooks compiles in the test-only gates the linked-worktree
  # semantic quiet-window test holds; without the feature that test is not built.
  run_phase "cargo test -p agent-file-tools --features test-timing-hooks --test watcher_integration --quiet -- --test-threads=1" \
    gate_cargo test -p agent-file-tools --features test-timing-hooks --test watcher_integration --quiet -- --test-threads=1
fi

if phase_enabled conformance; then
  run_phase "cargo test -p agent-file-tools --features test-timing-hooks --test rest -- scope_end_kill" \
    gate_cargo test -p agent-file-tools --features test-timing-hooks --test rest -- scope_end_kill
  run_phase "cargo test -p agent-file-tools --features test-timing-hooks --test integration -- scope_revocation" \
    gate_cargo test -p agent-file-tools --features test-timing-hooks --test integration -- scope_revocation
  run_phase "cargo test -p agent-file-tools --features test-timing-hooks --test rest -- tool_provider_conformance" \
    gate_cargo test -p agent-file-tools --features test-timing-hooks --test rest -- tool_provider_conformance
  # Keep promotion failure, deadline handoff, and elicitation TTL coverage in
  # the hook-enabled phase; ordinary production builds cannot inject them.
  run_phase "cargo test -p agent-file-tools --features test-timing-hooks --test integration -- subc_bridge_bash_promote subc_bridge_bash_started_promotion subc_bridge_bash_late_promote subc_bridge_untrusted_bash_elicitation_ttl" \
    gate_cargo test -p agent-file-tools --features test-timing-hooks --test integration -- subc_bridge_bash_promote subc_bridge_bash_started_promotion subc_bridge_bash_late_promote subc_bridge_untrusted_bash_elicitation_ttl
fi

# The main subc storm test asserts production-calibrated absolute latencies
# (2s bind headroom, the module's real 12s bind deadline). It is
# debug-ignored because an unoptimized build under load cannot honor those
# bounds even when the code is correct; the release profile is the
# authoritative calibration (measured ~14s for the whole storm suite).
# Skippable because the 2-core Windows CI runner can neither afford the
# cold release-profile build inside the job timeout nor honor absolute
# latency bounds — Linux and macOS CI remain the release-storm arbiters.
if phase_enabled storm; then
  if [[ "${AFT_GATE_SKIP_RELEASE_STORM:-}" == "1" ]]; then
    announce_gate_line "==> release-storm phase skipped (AFT_GATE_SKIP_RELEASE_STORM=1)"
  else
    # Storm is intentionally on the retry-free default nextest profile: a retry
    # would hide a real latency regression behind a second warm attempt.
    run_phase "cargo nextest run --profile default --cargo-profile release -E 'test(subc_storm)' (release-calibrated latency bounds)" \
      gate_cargo nextest run --profile default --cargo-profile release -p agent-file-tools --test integration -E 'test(subc_storm)'
  fi
fi

finish_gate
