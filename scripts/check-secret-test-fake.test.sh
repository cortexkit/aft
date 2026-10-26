#!/usr/bin/env bash
# Proves `scripts/check-secret-test-fake.sh` fails when the fake would ship.
#
# Runs the guard against a throwaway copy of the workspace manifests, with one
# change each time: a dependency edge that requests the feature, a `default`
# feature that names it, and another crate declaring its own. A guard that
# never fails proves nothing, so this test is what makes the CI step above it
# meaningful.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

guard="$repo_root/scripts/check-secret-test-fake.sh"

work="$(mktemp -d "${TMPDIR:-/tmp}/secret-fake-guard.XXXXXX")"
cleanup() { rm -rf "$work"; }
trap cleanup EXIT

# The guard asks git for the repository root; give the copy its own.
copy_manifests() {
  local into="$1"
  rm -rf "$into"
  mkdir -p "$into"
  git -C "$repo_root" init -q "$into"
  # Only the manifests matter; keep their directory shape.
  ( cd "$repo_root" && git ls-files '*Cargo.toml' | while IFS= read -r file; do
      mkdir -p "$into/$(dirname "$file")"
      cp "$file" "$into/$file"
    done )
}

expect_ok() {
  local label="$1" dir="$2"
  if ! ( cd "$dir" && bash "$guard" >/dev/null 2>&1 ); then
    printf 'self-test: %s should pass the guard but failed\n' "$label" >&2
    exit 1
  fi
}

expect_fail() {
  local label="$1" dir="$2"
  if ( cd "$dir" && bash "$guard" >/dev/null 2>&1 ); then
    printf 'self-test: %s should fail the guard but passed\n' "$label" >&2
    exit 1
  fi
}

clean="$work/clean"
copy_manifests "$clean"
expect_ok "the committed manifests" "$clean"

# 1. A dependency edge in another crate requests the fake.
edge="$work/edge"
copy_manifests "$edge"
printf '\n[dev-dependencies]\nagent-file-tools = { path = "../aft", features = ["secret-test-fake"] }\n' \
  >>"$edge/crates/aft-tokenizer/Cargo.toml"
expect_fail "a dependency edge requesting the fake" "$edge"

# 2. A default feature set names the fake.
default="$work/default"
copy_manifests "$default"
perl -0pi -e 's/\[features\]\n/[features]\ndefault = ["secret-test-fake"]\n/' \
  "$default/crates/aft/Cargo.toml"
expect_fail "a default feature naming the fake" "$default"

# 3. Another crate declares its own feature of the same name.
impostor="$work/impostor"
copy_manifests "$impostor"
printf '\n[features]\nsecret-test-fake = []\n' >>"$impostor/crates/aft-tokenizer/Cargo.toml"
expect_fail "an impostor feature in another crate" "$impostor"

printf 'secret-test-fake guard self-test: ok (1 pass, 3 failures)\n'
