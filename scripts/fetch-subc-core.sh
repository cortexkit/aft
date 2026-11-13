#!/usr/bin/env bash
set -euo pipefail

# Held subscriptions from SDK 0.22.8+ need daemon 0.20.40+; the AFT module's
# protocol floor is higher, at daemon 0.20.154.
SUBC_CORE_TAG="subc-core-v0.21.3"
SUBC_CORE_WIRE_VERSION="2"
SUBC_REPO="cortexkit/subconscious"
BIN_NAME="ck-subc"
CACHE_DIR="$HOME/.cache/aft-ci/subc-core/$SUBC_CORE_TAG"

fail() {
  echo "$*" >&2
  exit 1
}

resolve_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os/$arch" in
    Darwin/arm64|Darwin/aarch64)
      printf '%s\n' 'darwin-arm64'
      ;;
    Linux/x86_64|Linux/amd64)
      printf '%s\n' 'linux-x64'
      ;;
    *)
      fail "Unsupported subc-core target for this fetch script: $os/$arch (supported: Darwin/arm64, Linux/x86_64)"
      ;;
  esac
}

resolve_sha256_tool() {
  if command -v sha256sum >/dev/null 2>&1; then
    printf '%s\n' 'sha256sum'
    return
  fi
  if command -v shasum >/dev/null 2>&1; then
    printf '%s\n' 'shasum -a 256'
    return
  fi
  fail 'Neither sha256sum nor shasum -a 256 is available'
}

sha256_file() {
  local file="$1"
  case "$SHA256_TOOL" in
    sha256sum)
      sha256sum "$file" | awk '{print $1}'
      ;;
    'shasum -a 256')
      shasum -a 256 "$file" | awk '{print $1}'
      ;;
    *)
      fail "Unsupported SHA-256 tool selector: $SHA256_TOOL"
      ;;
  esac
}

read_expected_sha() {
  local sidecar="$1"
  awk 'NR == 1 { print $1; exit }' "$sidecar"
}

sidecar_matches() {
  local file="$1"
  local sidecar="$2"
  local expected actual
  expected="$(read_expected_sha "$sidecar")"
  [[ "$expected" == "$EXPECTED_SHA256" ]] || return 1
  actual="$(sha256_file "$file")"
  [[ "$actual" == "$expected" ]]
}

verify_or_fail() {
  local file="$1"
  local sidecar="$2"
  local expected actual
  expected="$(read_expected_sha "$sidecar")"
  [[ "$expected" =~ ^[0-9a-fA-F]{64}$ ]] || fail "Invalid SHA-256 sidecar format: $sidecar"
  actual="$(sha256_file "$file")"
  if [[ "$actual" != "$expected" ]]; then
    fail "SHA-256 mismatch for $file: expected $expected, got $actual"
  fi
}

extract_cached_binary() {
  local tarball="$1"
  local destination_dir="$2"
  local temp_extract
  temp_extract="$(mktemp -d "${TMPDIR:-/tmp}/subc-core-extract.XXXXXX")"
  trap 'rm -rf "$temp_extract"' RETURN
  unzip -q "$tarball" -d "$temp_extract"
  [[ -f "$temp_extract/$BIN_NAME" ]] || fail "Archive $tarball did not contain $BIN_NAME"
  mkdir -p "$destination_dir"
  cp "$temp_extract/$BIN_NAME" "$destination_dir/$BIN_NAME"
  chmod +x "$destination_dir/$BIN_NAME"
  rm -rf "$temp_extract"
  trap - RETURN
}

self_test() (
  local test_dir expected sidecar
  test_dir="$(mktemp -d "${TMPDIR:-/tmp}/subc-core-checksum-test.XXXXXX")"
  trap 'rm -rf "$test_dir"' EXIT
  SHA256_TOOL="$(resolve_sha256_tool)"
  expected="e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
  EXPECTED_SHA256="$expected"
  : > "$test_dir/probe.zip"
  for form in bare filename; do
    sidecar="$test_dir/$form.sha256"
    if [[ "$form" == bare ]]; then
      printf '%s\n' "$expected" > "$sidecar"
    else
      printf '%s  probe.zip\n' "$expected" > "$sidecar"
    fi
    [[ "$(read_expected_sha "$sidecar")" == "$expected" ]] || fail "Could not read $form checksum sidecar"
    sidecar_matches "$test_dir/probe.zip" "$sidecar" || fail "Could not match $form checksum sidecar"
    verify_or_fail "$test_dir/probe.zip" "$sidecar"
  done
  echo 'fetch-subc-core self-test: 2 checksum formats passed'
)

case "${1:-}" in
  --print-tag) printf '%s\n' "$SUBC_CORE_TAG"; exit 0 ;;
  --print-wire-version) printf '%s\n' "$SUBC_CORE_WIRE_VERSION"; exit 0 ;;
  --self-test) self_test; exit 0 ;;
esac

TARGET="$(resolve_target)"
case "$TARGET" in
  darwin-arm64)
    EXPECTED_SHA256="03da4455a8653356ee58b06abdafd713d47cfcd89aa42cbc1e5077269bbb193e"
    ;;
  linux-x64)
    EXPECTED_SHA256="2d5e01bf027e7ad2f74ce2907c33b484c3017c64520ac4da84795e69c39aad79"
    ;;
esac
SHA256_TOOL="$(resolve_sha256_tool)"
TARBALL_NAME="$BIN_NAME-$TARGET.zip"
SIDECAR_NAME="$TARBALL_NAME.sha256"
CACHED_TARBALL="$CACHE_DIR/$TARBALL_NAME"
CACHED_SIDECAR="$CACHE_DIR/$SIDECAR_NAME"
CACHED_BINARY="$CACHE_DIR/$BIN_NAME"

if [[ -f "$CACHED_TARBALL" && -f "$CACHED_SIDECAR" ]] && sidecar_matches "$CACHED_TARBALL" "$CACHED_SIDECAR"; then
  if [[ -x "$CACHED_BINARY" ]]; then
    printf '%s\n' "$CACHED_BINARY"
    exit 0
  fi
  extract_cached_binary "$CACHED_TARBALL" "$CACHE_DIR"
  printf '%s\n' "$CACHED_BINARY"
  exit 0
fi

command -v gh >/dev/null 2>&1 || fail 'GitHub CLI (gh) is required to fetch subc-core'

stage_dir="$(mktemp -d "${TMPDIR:-/tmp}/subc-core-fetch.XXXXXX")"
trap 'rm -rf "$stage_dir"' EXIT

echo "Fetching $TARBALL_NAME from $SUBC_REPO@$SUBC_CORE_TAG" >&2
# A CI runner's connection to api.github.com can drop for a moment, and one
# failed download would otherwise fail the whole job. The checksum below still
# verifies whatever a retry downloads.
max_attempts=4
attempt=1
delay=5
until gh release download "$SUBC_CORE_TAG" \
  --repo "$SUBC_REPO" \
  --dir "$stage_dir" \
  --pattern "$TARBALL_NAME"; do
  if (( attempt >= max_attempts )); then
    fail "Could not download $TARBALL_NAME from $SUBC_REPO@$SUBC_CORE_TAG after $max_attempts attempts"
  fi
  echo "Download attempt $attempt of $max_attempts failed; retrying in ${delay}s" >&2
  rm -f "$stage_dir/$TARBALL_NAME"
  sleep "$delay"
  attempt=$((attempt + 1))
  delay=$((delay * 3))
done

[[ -f "$stage_dir/$TARBALL_NAME" ]] || fail "Missing downloaded asset: $TARBALL_NAME"
printf '%s\n' "$EXPECTED_SHA256" > "$stage_dir/$SIDECAR_NAME"
verify_or_fail "$stage_dir/$TARBALL_NAME" "$stage_dir/$SIDECAR_NAME"
extract_cached_binary "$stage_dir/$TARBALL_NAME" "$stage_dir"

mkdir -p "$CACHE_DIR"
cp "$stage_dir/$TARBALL_NAME" "$CACHED_TARBALL"
cp "$stage_dir/$SIDECAR_NAME" "$CACHED_SIDECAR"
cp "$stage_dir/$BIN_NAME" "$CACHED_BINARY"
chmod +x "$CACHED_BINARY"

printf '%s\n' "$CACHED_BINARY"
