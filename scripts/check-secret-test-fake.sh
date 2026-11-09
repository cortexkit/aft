#!/usr/bin/env bash
# A release build must never compile the test-only secret vault or fixture hooks.
#
# The feature `secret-test-fake` compiles a vault that hands out key material on
# request. If a shipped build enabled it, a released binary could substitute
# fake keys. The fixture hooks allow test inputs and executable overrides.
# Cargo profiles cannot enable crate features, so this guard checks default
# feature sets and dependency edges that could enable either test-only feature.
#
# It reads the workspace manifests directly. `cargo metadata` would need the
# network on a clean runner, and this check must run before any build artifact
# exists.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

status=0

fail() {
  printf 'secret-test-fake guard: %s\n' "$1" >&2
  status=1
}

# `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]` and their
# `[target.'cfg(...)'.dependencies]` forms are where an edge can request the
# feature for the whole graph.
dependency_lines() {
  local manifest="$1"
  awk '
    /^[[:space:]]*\[/ { section = $0; next }
    section ~ /dependencies/ { print }
  ' "$manifest"
}

# Both test-only features must stay standalone opt-in switches, with no default
# or dependency edge enabling them in a production build.
for feature in secret-test-fake test-hooks; do
fake_owner="./crates/aft/Cargo.toml"
if [[ ! -f "$fake_owner" ]]; then
  fail "$fake_owner is missing, so $feature cannot be checked"
else
  if ! grep -Eq "^[[:space:]]*$feature[[:space:]]*=[[:space:]]*\[[[:space:]]*\][[:space:]]*$" "$fake_owner"; then
    fail "$fake_owner must declare $feature = [] as a standalone opt-in feature"
  fi
  # A `default` line that names the fake turns it on for every ordinary build.
  while IFS= read -r default_line; do
    [ -n "$default_line" ] || continue
    if grep -q "$feature" <<<"$default_line"; then
      fail "$fake_owner enables $feature from a default feature set: $default_line"
    fi
  done < <(grep -E '^[[:space:]]*default[[:space:]]*=' "$fake_owner" || true)
fi

# No other crate may declare the feature (an impostor with its own meaning), and
# no dependency edge may request it from `agent-file-tools`.
checked=0
while IFS= read -r manifest; do
  checked=$((checked + 1))
  if [[ "$manifest" != "$fake_owner" ]]; then
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      case "$line" in
        \#*) continue ;;
      esac
      if [[ "$line" =~ ^[[:space:]]*$feature[[:space:]]*= ]]; then
        fail "$manifest declares its own $feature feature"
      fi
    done < <(grep -E "^[[:space:]]*$feature[[:space:]]*=" "$manifest" || true)
  fi
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    case "$line" in
      \#*) continue ;;
    esac
    if grep -q "\"$feature\"" <<<"$line"; then
      fail "$manifest requests $feature from a dependency: $line"
    fi
  done < <(dependency_lines "$manifest" | grep -E 'agent-file-tools' || true)
done < <(find . -name Cargo.toml -not -path './target/*' -not -path './node_modules/*' | sort)
done

if [[ "$status" -ne 0 ]]; then
  printf 'secret-test-fake guard: FAILED\n' >&2
  exit 1
fi
printf 'secret-test-fake guard: ok (%d manifests, secret-test-fake and test-hooks stay opt-in)\n' "$checked"
