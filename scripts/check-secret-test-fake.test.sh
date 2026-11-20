#!/usr/bin/env bash
# Prove the guard admits explicit test opt-ins but rejects release activation.
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
guard="$repo_root/scripts/check-secret-test-fake.sh"
fixtures="$repo_root/scripts/fixtures/check-secret-test-fake"

mkdir -p "$repo_root/target/scratch"
work="$(mktemp -d "$repo_root/target/scratch/secret-fake-guard.XXXXXX")"
trap 'rm -rf "$work"' EXIT

write_package() {
  local directory="$1" package="$2"
  mkdir -p "$directory/src"
  printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n' "$package" >"$directory/Cargo.toml"
  touch "$directory/src/lib.rs"
}

make_workspace() {
  local directory="$1"
  mkdir -p "$directory"
  git init -q "$directory"
  printf '%s\n' '[workspace]' 'resolver = "2"' \
    'members = ["aft", "aft-base", "aft-tokenizer", "candidate"]' \
    '[workspace.dependencies]' 'base = { package = "aft-base", path = "aft-base" }' \
    >"$directory/Cargo.toml"
  write_package "$directory/aft" agent-file-tools
  write_package "$directory/aft-base" aft-base
  write_package "$directory/aft-tokenizer" aft-tokenizer
  write_package "$directory/candidate" candidate
  printf '\n[features]\nsecret-test-fake = []\ntest-hooks = []\ntest-support = []\n' \
    >>"$directory/aft-base/Cargo.toml"
  printf '\n[features]\nsecret-test-fake = []\ntest-hooks = []\n' >>"$directory/aft/Cargo.toml"
}

passed=0
rejected=0
expect_ok() {
  local label="$1" directory="$2" output
  if ! output=$(cd "$directory" && bash "$guard" 2>&1); then
    printf 'self-test: %s should pass the guard but failed:\n%s\n' "$label" "$output" >&2
    exit 1
  fi
  passed=$((passed + 1))
}

expect_fail() {
  local label="$1" directory="$2" expected="$3" output
  if output=$(cd "$directory" && bash "$guard" 2>&1); then
    printf 'self-test: %s should fail the guard but passed\n' "$label" >&2
    exit 1
  fi
  if [[ "$output" != *"$expected"* ]]; then
    printf 'self-test: %s failed for the wrong reason; expected %s:\n%s\n' "$label" "$expected" "$output" >&2
    exit 1
  fi
  rejected=$((rejected + 1))
}

expect_ok 'the committed manifests' "$repo_root"

# Check dev-only requests, default sets, and declarations by another member
# for both original flags. Only the default and normal dependency may leak.
for feature in secret-test-fake test-hooks; do
  edge="$work/dev-$feature"
  make_workspace "$edge"
  printf '\n[dev-dependencies]\nagent-file-tools = { path = "../aft", features = ["%s"] }\n' "$feature" \
    >>"$edge/aft-tokenizer/Cargo.toml"
  expect_ok "a dev dependency requesting $feature" "$edge"

  edge="$work/normal-$feature"
  make_workspace "$edge"
  printf '\n[dependencies]\nagent-file-tools = { path = "../aft", features = ["%s"] }\n' "$feature" \
    >>"$edge/aft-tokenizer/Cargo.toml"
  expect_fail "a normal dependency requesting $feature" "$edge" \
    "non-dev dependency agent-file-tools requests protected feature $feature"

  default="$work/default-$feature"
  make_workspace "$default"
  printf 'default = ["%s"]\n' "$feature" >>"$default/aft/Cargo.toml"
  expect_fail "a default naming $feature" "$default" "default enables a protected feature: $feature"

  declaration="$work/declaration-$feature"
  make_workspace "$declaration"
  printf '\n[features]\n%s = []\n' "$feature" >>"$declaration/aft-tokenizer/Cargo.toml"
  expect_ok "an opt-in declaration of $feature on another workspace crate" "$declaration"
done

allowed="$work/allowed-forwards"
make_workspace "$allowed"
printf '%s\n' '[dependencies]' 'base = { workspace = true, optional = true }' \
  '[features]' 'test-hooks = ["base?/test-support"]' 'test-support = ["dep:base", "base/test-hooks"]' \
  >>"$allowed/candidate/Cargo.toml"
expect_ok 'cross-feature forwarding to an optional, renamed workspace sibling' "$allowed"

bad_manifest() {
  local fixture="$1" diagnostic="$2" directory="$work/$1"
  make_workspace "$directory"
  cp "$fixtures/$fixture.toml" "$directory/candidate/Cargo.toml"
  expect_fail "$fixture" "$directory" "$diagnostic"
}

bad_manifest default-test-support 'default enables a protected feature: test-support'
bad_manifest default-forward 'default enables a protected feature: base/test-hooks'
bad_manifest non-dev-edge 'non-dev dependency base requests protected feature test-support'
bad_manifest external-forward 'test-hooks must forward aft-base/test-hooks to a declared protected feature of a workspace sibling'
bad_manifest ordinary-forward 'ordinary feature release-ready enables a protected feature: aft-base?/test-support'
bad_manifest strict-prerequisite 'test-hooks may only forward to a protected workspace feature: dep:base'

outside="$work/outside"
make_workspace "$outside"
write_package "$outside/outside" outside
cp "$fixtures/non-member.toml" "$outside/outside/Cargo.toml"
expect_fail 'a protected declaration in a non-member crate' "$outside" \
  'non-workspace crate declares protected feature test-hooks'

printf 'secret-test-fake guard self-test: ok (%d pass, %d expected rejections)\n' "$passed" "$rejected"
