#!/usr/bin/env bash
set -euo pipefail

script="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)/windows-gate.sh"
fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }

test_non_windows_host_refuses_with_runon_windows_guidance() {
  local temp output code
  temp=$(mktemp -d)
  cat > "$temp/uname" <<'SH'
#!/bin/sh
printf 'Linux\n'
SH
  chmod +x "$temp/uname"
  set +e
  output=$(PATH="$temp:$PATH" bash "$script" --plan --filter search_index:: 2>&1)
  code=$?
  set -e
  rm -rf "$temp"
  [[ $code -eq 2 && "$output" == *'runon "windows"'* ]] || fail "${FUNCNAME[0]}"
  printf 'PASS: %s\n' "${FUNCNAME[0]}"
}

test_empty_test_run_is_rejected() {
  local temp output code
  temp=$(mktemp -d)
  cat > "$temp/uname" <<'SH'
#!/bin/sh
printf 'MINGW64_NT-10.0\n'
SH
  cat > "$temp/cygpath" <<'SH'
#!/bin/sh
for path do last=$path; done
printf '%s\n' "$last"
SH
  cat > "$temp/timeout" <<'SH'
#!/bin/sh
shift
exec "$@"
SH
  cat > "$temp/cargo" <<'SH'
#!/bin/sh
case " $* " in
  *gate_resolves_every_user_config_and_state_path_under_the_gate_homes*)
    printf 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n' ;;
  *)
    printf 'running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 15 filtered out\n' ;;
esac
SH
  chmod +x "$temp/uname" "$temp/cygpath" "$temp/timeout" "$temp/cargo"
  set +e
  output=$(PATH="$temp:$PATH" bash "$script" --filter missing_test_name 2>&1)
  code=$?
  set -e
  rm -rf "$temp"
  [[ $code -ne 0 && "$output" == *'GATE ERROR: Zero tests executed'* && "$output" == *'GATE FAILED'* ]] || fail "${FUNCNAME[0]}"
  printf 'PASS: %s\n' "${FUNCNAME[0]}"
}

test_untracked_rust_changes_select_the_module_slice() {
  local temp output
  temp=$(mktemp -d)
  cat > "$temp/uname" <<'SH'
#!/bin/sh
printf 'MINGW64_NT-10.0\n'
SH
  cat > "$temp/git" <<'SH'
#!/bin/sh
case "$1" in
  rev-parse) printf '0000000000000000000000000000000000000000\n' ;;
  diff) : ;;
  ls-files) printf 'crates/aft/src/search_index.rs\0' ;;
esac
SH
  chmod +x "$temp/uname" "$temp/git"
  output=$(PATH="$temp:$PATH" bash "$script" --plan 2>&1)
  [[ "$output" == *'--lib search_index::'* && "$output" != *'--features test-hooks'* ]] || fail "${FUNCNAME[0]}"
  output=$(PATH="$temp:$PATH" bash "$script" --plan --test-target integration --features test-timing-hooks 2>&1)
  [[ "$output" == *'--test integration --features test-hooks --features test-timing-hooks'* ]] || fail "${FUNCNAME[0]} integration hooks"
  rm -rf "$temp"
  printf 'PASS: %s\n' "${FUNCNAME[0]}"
}

test_non_windows_host_refuses_with_runon_windows_guidance
test_empty_test_run_is_rejected
test_untracked_rust_changes_select_the_module_slice
