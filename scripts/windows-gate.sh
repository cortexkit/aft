#!/usr/bin/env bash
# Run the native x64 Windows test gate from Git Bash.
set -euo pipefail

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) ;;
  *)
    printf 'Run it with runon "windows" (Git Bash on Windows is required).\n' >&2
    exit 2
    ;;
esac

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
cd "$repo_root"

base=origin/main
full=0
filter=
filter_set=0
test_target=
features=
timeout_minutes=60
show_status=0
plan_only=0
while (($#)); do
  case "$1" in
    --base)
      if (($# < 2)); then echo "--base requires a revision" >&2; exit 2; fi
      base=$2; shift 2 ;;
    --full) full=1; shift ;;
    --filter)
      if (($# < 2)); then echo "--filter requires a test filter" >&2; exit 2; fi
      filter=$2; filter_set=1; shift 2 ;;
    --test-target)
      if (($# < 2)) || [[ -z "$2" ]]; then echo "--test-target requires a target" >&2; exit 2; fi
      test_target=$2; shift 2 ;;
    --features)
      if (($# < 2)) || [[ -z "$2" ]]; then echo "--features requires a feature list" >&2; exit 2; fi
      features=$2; shift 2 ;;
    --timeout-minutes)
      if (($# < 2)); then echo "--timeout-minutes requires a positive integer" >&2; exit 2; fi
      timeout_minutes=$2; shift 2 ;;
    --status) show_status=1; shift ;;
    --plan) plan_only=1; shift ;;
    --help|-h) echo 'Run with runon windows; options: --base, --full, --filter, --test-target, --features, --timeout-minutes, --status, --plan'; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done


if [[ ! "$timeout_minutes" =~ ^[0-9]+$ ]]; then
  echo "timeout must be a positive integer" >&2
  exit 2
fi
timeout_minutes=$((10#$timeout_minutes))
if ((timeout_minutes < 1)); then
  echo "timeout must be a positive integer" >&2
  exit 2
fi
if ((filter_set)) && [[ -z "$filter" ]]; then
  echo "filter must not be empty" >&2
  exit 2
fi
if ((full)) && [[ -n "$test_target" ]]; then
  echo '--full and --test-target cannot be combined' >&2
  exit 2
fi

if ((show_status)); then
  printf 'Windows native test runner\nCargo target directory: %s\n' "${CARGO_TARGET_DIR:-$repo_root/target}"
  df -h "${CARGO_TARGET_DIR:-$repo_root/target}" 2>/dev/null || df -h "$repo_root"
  exit 0
fi

job_kinds=()
job_targets=()
job_filters=()
add_job() {
  local kind=$1 target=$2 test_filter=$3 i
  for ((i = 0; i < ${#job_kinds[@]}; i++)); do
    if [[ ${job_kinds[$i]} == "$kind" && ${job_targets[$i]} == "$target" && ${job_filters[$i]} == "$test_filter" ]]; then
      return 0
    fi
  done
  job_kinds+=("$kind")
  job_targets+=("$target")
  job_filters+=("$test_filter")
}
select_path() {
  local path=$1 rest target test_file module test_filter
  case "$path" in
    Cargo.toml|Cargo.lock|build.rs|crates/aft/Cargo.toml|crates/aft/build.rs|crates/aft/src/lib.rs|crates/aft/src/context.rs|crates/aft/src/db.rs|crates/aft/src/executor.rs|crates/aft/src/config*.rs|crates/aft/src/subc_config.rs|crates/aft/src/executor/*|crates/aft/src/db/*|crates/aft-tokenizer/*)
      shared=1 ;;
    crates/aft/src/main.rs)
      add_job bin aft '' ;;
    crates/aft/src/*.rs)
      module=${path#crates/aft/src/}; module=${module%%/*}; module=${module%.rs}
      add_job lib '' "$module::" ;;
    crates/aft/tests/*.rs)
      rest=${path#crates/aft/tests/}; target=${rest%%/*}; test_file=${rest##*/}
      if [[ "$target" == helpers || "$target" == fixtures ]]; then
        add_job test integration ''
      elif [[ "$rest" != */* ]]; then
        add_job test "${rest%.rs}" ''
      elif [[ "$rest" == */*/* || "$test_file" == main.rs || "$rest" == */helpers/* ]]; then
        add_job test "$target" ''
      else
        test_filter=${test_file%.rs}
        add_job test "$target" "$test_filter::"
      fi ;;
  esac
}

if [[ -n "$test_target" ]]; then
  plan_label="explicit test target $test_target"
elif [[ -n "$filter" ]]; then
  plan_label='explicit filter'
else
  base_sha="$(git rev-parse --verify "$base^{commit}")"
  plan_label="$base_sha..working tree"
  shared=0
  while IFS= read -r -d '' changed_path; do select_path "$changed_path"; done < <(git diff --name-only -z "$base_sha")
  while IFS= read -r -d '' changed_path; do select_path "$changed_path"; done < <(git ls-files --others --exclude-standard -z)
  if ((shared)); then
    kept_kinds=(); kept_targets=(); kept_filters=()
    for ((i = 0; i < ${#job_kinds[@]}; i++)); do
      if [[ ${job_kinds[$i]} != lib ]]; then
        kept_kinds+=("${job_kinds[$i]}"); kept_targets+=("${job_targets[$i]}"); kept_filters+=("${job_filters[$i]}")
      fi
    done
    job_kinds=(); job_targets=(); job_filters=()
    if ((${#kept_kinds[@]} > 0)); then
      job_kinds=("${kept_kinds[@]}"); job_targets=("${kept_targets[@]}"); job_filters=("${kept_filters[@]}")
    fi
    add_job lib '' ''
  fi
fi

if ((full)); then add_job lib '' ''; add_job test integration ''; fi
if [[ -n "$filter" ]]; then
  job_kinds=(); job_targets=(); job_filters=()
  add_job lib '' "$filter"
  if ((full)); then add_job test integration "$filter"; fi
fi
if [[ -n "$test_target" ]]; then
  job_kinds=(); job_targets=(); job_filters=()
  add_job test "$test_target" "$filter"
fi

selected_kinds=(); selected_targets=(); selected_filters=()
for ((i = 0; i < ${#job_kinds[@]}; i++)); do
  skip=0
  if [[ -n ${job_filters[$i]} ]]; then
    for ((j = 0; j < ${#job_kinds[@]}; j++)); do
      if [[ ${job_kinds[$i]} == "${job_kinds[$j]}" && ${job_targets[$i]} == "${job_targets[$j]}" && -z ${job_filters[$j]} ]]; then
        skip=1
        break
      fi
    done
  fi
  if ((!skip)); then
    selected_kinds+=("${job_kinds[$i]}"); selected_targets+=("${job_targets[$i]}"); selected_filters+=("${job_filters[$i]}")
  fi
done
job_kinds=(); job_targets=(); job_filters=()
if ((${#selected_kinds[@]} > 0)); then
  job_kinds=("${selected_kinds[@]}"); job_targets=("${selected_targets[@]}"); job_filters=("${selected_filters[@]}")
fi

printf 'Windows x64 plan: %s; cap %s min; jobs/test threads 4\n' "$plan_label" "$timeout_minutes"
if ((${#job_kinds[@]} == 0)); then
  echo 'No Rust tests selected; use --full or --filter for an explicit gate.'
  exit 0
fi
print_job() {
  local kind=$1 target=$2 test_filter=$3
  local -a args=(test --locked -j 4 -p agent-file-tools "--$kind")
  [[ -z "$target" ]] || args+=("$target")
  [[ -z "$features" ]] || args+=(--features "$features")
  [[ -z "$test_filter" ]] || args+=("$test_filter")
  args+=(-- --test-threads 4)
  printf '  cargo'; printf ' %q' "${args[@]}"; printf '\n'
}
printf '  storage preflight:\n'
print_job lib '' gate_hermeticity_tests::gate_resolves_every_user_config_and_state_path_under_the_gate_homes
for ((i = 0; i < ${#job_kinds[@]}; i++)); do print_job "${job_kinds[$i]}" "${job_targets[$i]}" "${job_filters[$i]}"; done
if ((plan_only)); then exit 0; fi

if ! command -v timeout >/dev/null 2>&1 || ! command -v cygpath >/dev/null 2>&1; then
  echo 'Git Bash must provide timeout and cygpath to run the native Windows gate.' >&2
  exit 2
fi

original_userprofile=${USERPROFILE:-$(cygpath -w "$HOME")}
original_userprofile_posix=$(cygpath -u "$original_userprofile")
export CARGO_HOME=${CARGO_HOME:-$(cygpath -w "$original_userprofile_posix/.cargo")}
export RUSTUP_HOME=${RUSTUP_HOME:-$(cygpath -w "$original_userprofile_posix/.rustup")}
gate_tmp=$(mktemp -d "${TMPDIR:-/tmp}/aft-windows-gate.XXXXXX")
cleanup() { rm -rf "$gate_tmp"; }
trap cleanup EXIT

while IFS='=' read -r name _; do
  case "$name" in AFT_*|XDG_*|GIT_CONFIG_*|GIT_DIR|GIT_WORK_TREE) unset "$name" ;; esac
done < <(env)
for directory in home appdata localappdata data config cache state temp; do
  mkdir -p "$gate_tmp/$directory"
done
HOME="$(cygpath -w "$gate_tmp/home")"
USERPROFILE=$HOME
APPDATA="$(cygpath -w "$gate_tmp/appdata")"
LOCALAPPDATA="$(cygpath -w "$gate_tmp/localappdata")"
XDG_DATA_HOME="$(cygpath -w "$gate_tmp/data")"
XDG_CONFIG_HOME="$(cygpath -w "$gate_tmp/config")"
XDG_CACHE_HOME="$(cygpath -w "$gate_tmp/cache")"
XDG_STATE_HOME="$(cygpath -w "$gate_tmp/state")"
TEMP="$(cygpath -w "$gate_tmp/temp")"
TMP=$TEMP
export HOME USERPROFILE APPDATA LOCALAPPDATA XDG_DATA_HOME XDG_CONFIG_HOME XDG_CACHE_HOME XDG_STATE_HOME TEMP TMP
export HOMEDRIVE=${USERPROFILE:0:2}
export HOMEPATH=${USERPROFILE:2}
AFT_GATE_HERMETIC_HOME_ROOT="$(cygpath -w "$gate_tmp")"
export AFT_RUST_TEST_GATE=1 AFT_GATE_HERMETIC_HOME_ROOT
export RUST_BACKTRACE=1 CARGO_TERM_COLOR=never CARGO_BUILD_JOBS=4 GIT_CONFIG_NOSYSTEM=1
mkdir -p "$gate_tmp/home"
cat > "$gate_tmp/home/.gitconfig" <<'GITCONFIG'
[user]
  name = AFT Windows Gate
  email = windows-gate@example.invalid
[core]
  hooksPath = NUL
  autocrlf = false
GITCONFIG
GIT_CONFIG_GLOBAL="$(cygpath -w "$gate_tmp/home/.gitconfig")"
export GIT_CONFIG_GLOBAL

started=$SECONDS
run_job() {
  local kind=$1 target=$2 test_filter=$3 label=$4 remaining output code
  local -a args=(test --locked -j 4 -p agent-file-tools "--$kind")
  [[ -z "$target" ]] || args+=("$target")
  [[ -z "$features" ]] || args+=(--features "$features")
  [[ -z "$test_filter" ]] || args+=("$test_filter")
  args+=(-- --test-threads 4)
  remaining=$((timeout_minutes * 60 - (SECONDS - started)))
  if ((remaining <= 0)); then
    echo 'TIMEOUT: Windows test gate exceeded its wall-clock cap.'
    return 1
  fi
  output=$(mktemp "$gate_tmp/output.XXXXXX")
  printf '\n==> cargo'; printf ' %q' "${args[@]}"; printf '\n'
  if timeout "${remaining}s" cargo "${args[@]}" > "$output" 2>&1; then code=0; else code=$?; fi
  cat "$output"
  if ((code == 124 || code == 137)); then echo 'TIMEOUT: Windows test gate exceeded its wall-clock cap.'; fi
  if ! grep -Eq 'test result: .* [1-9][0-9]* (passed|failed);' "$output"; then
    printf 'GATE ERROR: Zero tests executed for %s; an empty slice is not a pass.\n' "$label"
    if ((code == 0)); then code=1; fi
  fi
  rm -f "$output"
  return "$code"
}

failed=0
run_job lib '' gate_hermeticity_tests::gate_resolves_every_user_config_and_state_path_under_the_gate_homes 'storage isolation preflight' || {
  echo 'Storage isolation preflight failed; refusing to run selected tests.'
  failed=1
}
if ((!failed)); then
  for ((i = 0; i < ${#job_kinds[@]}; i++)); do
    if ! run_job "${job_kinds[$i]}" "${job_targets[$i]}" "${job_filters[$i]}" "${job_kinds[$i]} ${job_targets[$i]} ${job_filters[$i]}"; then
      failed=1
    fi
  done
fi
if ((failed)); then echo 'GATE FAILED'; exit 1; fi
echo 'GATE PASSED'
