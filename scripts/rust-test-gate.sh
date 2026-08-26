#!/usr/bin/env bash
set -euo pipefail

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


# A test that exercises the plugin-less storage fallback must never inherit the
# operator's real data root. Keep the whole gate hermetic, including test binaries
# that launch child AFT processes without the shared integration helper.
unset AFT_CACHE_DIR
export XDG_DATA_HOME="${AFT_GATE_XDG_DATA_HOME:-${CARGO_TARGET_DIR:-$PWD/target}/aft-gate-data-home}"
mkdir -p "$XDG_DATA_HOME"

runner="${AFT_RUST_TEST_RUNNER:-nextest}"
unit_runner="${AFT_UNIT_TEST_RUNNER:-cargo}"

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
      started=$(python3 -c "import json,sys;print(json.load(open('$box_gate_lock')).get('started_at',0))" 2>/dev/null || echo 0)
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

run_phase() {
  local label="$1"
  shift
  local started=$SECONDS

  echo "==> $label"
  "$@"
  echo "    ok ($((SECONDS - started))s)"
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
  codesign -f -s - --identifier aft-dev-gate "$binary" >/dev/null 2>&1 || true
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

if [[ "$runner" == "cargo" ]]; then
  if [[ "$(uname)" == "Darwin" && "${AFT_GATE_NO_XPROTECT_REMEDIATION:-}" != "1" ]]; then
    run_phase "warm macOS Gatekeeper assessment: sign + exec debug test binaries" \
      warm_macos_test_binaries --workspace
  fi
  exec cargo test --workspace --quiet
fi

if [[ "$runner" != "nextest" ]]; then
  echo "Unsupported AFT_RUST_TEST_RUNNER='$runner' (expected 'nextest' or 'cargo')" >&2
  exit 2
fi
if [[ "$unit_runner" != "cargo" && "$unit_runner" != "nextest" ]]; then
  echo "Unsupported AFT_UNIT_TEST_RUNNER='$unit_runner' (expected 'cargo' or 'nextest')" >&2
  exit 2
fi

if ! command -v cargo-nextest >/dev/null 2>&1; then
  echo "cargo-nextest is required; install it with: cargo install cargo-nextest --locked" >&2
  exit 127
fi

extract_nextest_archive_for_prewarm() {
  # Newer cargo-nextest (>= ~0.9.13x) refuses --extract-to when the
  # destination already holds a target/ tree (older versions merged into
  # it). CI restores a cached target/ into the workspace before this step
  # runs, so clear the collision: the archive fully replaces that tree.
  if [ -d "$AFT_NEXTEST_EXTRACT_TO/target" ]; then
    rm -rf "$AFT_NEXTEST_EXTRACT_TO/target"
  fi
  cargo nextest list \
    --archive-file "$AFT_NEXTEST_ARCHIVE_FILE" \
    --extract-to "$AFT_NEXTEST_EXTRACT_TO" \
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
# is named and killed by the unit profile instead of leaving one silent libtest
# process until the surrounding job times out.
#
# CI can split the independent execution phases after one job creates a nextest
# archive. The default remains `all`, preserving the single-machine gate used
# locally and by workflows that do not opt into sharding.
requested_phases="${AFT_GATE_PHASES:-all}"
if [[ "$requested_phases" == "all" ]]; then
  phase_enabled() { return 0; }
else
  IFS=',' read -r -a selected_phases <<< "$requested_phases"
  for selected_phase in "${selected_phases[@]}"; do
    case "$selected_phase" in
      lib|nextest|watcher|storm) ;;
      *)
        echo "Unsupported AFT_GATE_PHASES entry '$selected_phase' (expected lib, nextest, watcher, storm, or all)" >&2
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

if phase_enabled lib; then
  # The platform-verifier TLS test spawns a subprocess whose keychain trust
  # evaluation is unbounded on Macs with third-party root CAs (NordVPN: ~10s
  # quiet, minutes under full-suite load — blew a 600s budget twice on
  # 2026-08-09). Isolation is the fix, not a bigger budget: run it alone
  # first (seconds when serial), then exclude it from the parallel phase.
  run_phase "cargo test -p agent-file-tools --lib platform_verifier_tls_client_subprocess --quiet (serial: keychain-latency-sensitive)" \
    cargo test -p agent-file-tools --lib platform_verifier_tls_client_subprocess --quiet

  if [[ "$unit_runner" == "nextest" ]]; then
    run_phase "cargo nextest run --workspace --lib --bins --profile unit" \
      cargo nextest run --workspace --lib --bins --profile unit -- \
        --skip platform_verifier_tls_client_subprocess
  else
    # PROBE (branch-only): serial, named output - the log's last started test
    # is the one terminating the process on Windows.
    run_phase "cargo test --workspace --lib --bins (serial probe)" \
      cargo test --workspace --lib --bins -- \
        --test-threads=1 \
        --skip platform_verifier_tls_client_subprocess
  fi
fi

if phase_enabled nextest && [[ "$(uname)" == "Darwin" && -z "${AFT_NEXTEST_ARCHIVE_FILE:-}" && "${AFT_GATE_NO_XPROTECT_REMEDIATION:-}" != "1" ]]; then
  run_phase "warm macOS Gatekeeper assessment: sign + exec debug test binaries" \
    bash -c "$(declare -f warm_macos_executable)
      $(declare -f warm_macos_test_binaries)
      warm_macos_test_binaries --workspace"
fi

if phase_enabled nextest; then
  nextest_args=(cargo nextest run)
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
  run_phase "$nextest_label" "${nextest_args[@]}"
fi

if phase_enabled watcher; then
  run_phase "cargo test -p agent-file-tools --test watcher_integration --quiet -- --test-threads=1" \
    cargo test -p agent-file-tools --test watcher_integration --quiet -- --test-threads=1
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
    echo "==> release-storm phase skipped (AFT_GATE_SKIP_RELEASE_STORM=1)"
  else
    run_phase "cargo nextest run --cargo-profile release -E 'test(subc_storm)' (release-calibrated latency bounds)" \
      cargo nextest run --cargo-profile release -p agent-file-tools --test integration -E 'test(subc_storm)'
  fi
fi
