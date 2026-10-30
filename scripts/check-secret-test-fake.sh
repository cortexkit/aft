#!/usr/bin/env bash
# V5: a release build must never compile the test-only secret vault.
#
# The feature `secret-test-fake` compiles a vault that hands out key material on
# request. If a shipped build enabled it, a released binary could substitute
# fake keys. Cargo profiles cannot turn crate features on, so this guard checks
# the two places a build graph can: a feature that already requires
# `secret-test-fake` (which a profile would pick up through `--features`), and a
# dependency edge that requests it on behalf of another crate.
#
# It reads the workspace manifests directly. `cargo metadata` would need the
# network on a clean runner, and this check must run before any build artifact
# exists.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

feature="secret-test-fake"
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

# The feature block of the crate that owns the fake: `secret-test-fake` must
# stay a standalone, opt-in switch with no `default` path to it.
fake_owner="./crates/aft/Cargo.toml"
if [[ ! -f "$fake_owner" ]]; then
  fail "$fake_owner is missing, so the fake cannot be checked"
else
  if ! grep -Eq "^[[:space:]]*$feature[[:space:]]*=[[:space:]]*\[[[:space:]]*\][[:space:]]*$" "$fake_owner"; then
    fail "$fake_owner must declare $feature = [] with no other feature behind the fake"
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

if [[ "$status" -ne 0 ]]; then
  printf 'secret-test-fake guard: FAILED\n' >&2
  exit 1
fi
printf 'secret-test-fake guard: ok (%d manifests, %s stays opt-in)\n' "$checked" "$feature"
