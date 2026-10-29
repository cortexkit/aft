#!/usr/bin/env bash
# Install the pinned TypeScript releases that the real-server integration
# tests use (crates/aft/tests/integration/typescript_native_lsp_test.rs):
#   <dir>/ts7/node_modules  typescript 7 and its platform package, served by
#                           the native language server (tsc --lsp --stdio)
#   <dir>/ts5/node_modules  typescript 5 and typescript-language-server
# and export both paths through $GITHUB_ENV when it is set. A directory
# restored from the CI cache is verified and reused; anything incomplete is
# reinstalled. The tests fail, rather than skip, when these variables point at
# a missing install, so a broken step cannot hide the coverage.
#
# Changing a version here changes this file's hash, which is the cache key.
set -euo pipefail

TS7_VERSION="7.0.2"
TS5_VERSION="5.9.3"
TSLS_VERSION="6.0.1"

dest="${1:?usage: install-typescript-test-servers.sh <dir> [--offline]}"
# The prepush Rust phase passes --offline; CI keeps its network-enabled default.
install_flags=()
case "${2:-}" in
  '') ;;
  --offline) install_flags=(--offline) ;;
  *) echo 'usage: install-typescript-test-servers.sh <dir> [--offline]' >&2; exit 2 ;;
esac
mkdir -p "$dest"
dest="$(cd "$dest" && pwd)"

installed_version() {
  local package_json="$1"
  [ -f "$package_json" ] || return 0
  sed -n 's/^[[:space:]]*"version":[[:space:]]*"\([^"]*\)".*/\1/p' "$package_json" | head -n 1
}

install_into() {
  local dir="$1"
  shift
  rm -rf "$dir"
  mkdir -p "$dir"
  printf '{"name":"%s","private":true}\n' "$(basename "$dir")" >"$dir/package.json"
  (cd "$dir" && bun add ${install_flags[@]+"${install_flags[@]}"} --exact --dev "$@")
}

ts7="$dest/ts7"
if [ "$(installed_version "$ts7/node_modules/typescript/package.json")" != "$TS7_VERSION" ] ||
  ! ls "$ts7"/node_modules/@typescript/typescript-*/lib/tsc* >/dev/null 2>&1; then
  install_into "$ts7" "typescript@$TS7_VERSION"
fi
ls "$ts7"/node_modules/@typescript/typescript-*/lib/tsc* >/dev/null 2>&1 || {
  echo "typescript@$TS7_VERSION installed without its platform package in $ts7" >&2
  exit 1
}

ts5="$dest/ts5"
if [ "$(installed_version "$ts5/node_modules/typescript/package.json")" != "$TS5_VERSION" ] ||
  [ "$(installed_version "$ts5/node_modules/typescript-language-server/package.json")" != "$TSLS_VERSION" ]; then
  install_into "$ts5" "typescript@$TS5_VERSION" "typescript-language-server@$TSLS_VERSION"
fi
[ -f "$ts5/node_modules/typescript/lib/tsserver.js" ] || {
  echo "typescript@$TS5_VERSION in $ts5 has no lib/tsserver.js" >&2
  exit 1
}

echo "TypeScript $TS7_VERSION: $ts7/node_modules"
echo "TypeScript $TS5_VERSION + typescript-language-server $TSLS_VERSION: $ts5/node_modules"
if [ -n "${GITHUB_ENV:-}" ]; then
  {
    echo "AFT_TEST_TS7_NODE_MODULES=$ts7/node_modules"
    echo "AFT_TEST_TS5_NODE_MODULES=$ts5/node_modules"
  } >>"$GITHUB_ENV"
fi
