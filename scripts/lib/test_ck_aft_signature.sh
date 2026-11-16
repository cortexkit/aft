#!/usr/bin/env bash
# Native macOS regression checks using real Mach-O signatures, not mocked flags.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=ck-aft-signature.sh
source "$ROOT/scripts/lib/ck-aft-signature.sh"
mkdir -p "$ROOT/target/scratch"
work="$(mktemp -d "$ROOT/target/scratch/signature-test.XXXXXX")"
trap 'rm -rf "$work"' EXIT
export TMPDIR="$work"
printf 'int main(void) { return 0; }\n' > "$work/main.c"
clang "$work/main.c" -o "$work/original"
passed=0

accept() {
  local name="$1"
  shift
  if ! "$@" > "$work/check.log" 2>&1; then
    cat "$work/check.log" >&2
    echo "FAIL: $name" >&2
    exit 1
  fi
  passed=$((passed + 1))
  echo "PASS: $name"
}

reject() {
  local name="$1" reason="$2"
  shift 2
  if "$@" > "$work/check.log" 2>&1; then
    echo "FAIL: $name was accepted" >&2
    exit 1
  fi
  if ! grep -qF "$reason" "$work/check.log"; then
    cat "$work/check.log" >&2
    echo "FAIL: $name did not report $reason" >&2
    exit 1
  fi
  passed=$((passed + 1))
  echo "PASS: $name"
}

cp "$work/original" "$work/aft"
ck_aft_sign_npm "$work/aft"
accept 'npm hardened ad-hoc signature' ck_aft_check_npm_signature "$work/aft" test
accept 'signed npm executable starts' "$work/aft"
reject 'daemon rejects npm identifier' 'signature identifier is not ck-aft' ck_aft_check_signature "$work/aft" test

# Keep the daemon entry point team-only, even if an operator chooses ad hoc.
CK_AFT_SIGN_IDENTITY=- ck_aft_sign "$work/aft"
reject 'daemon rejects ad hoc with ck-aft identifier' 'signed ad hoc (no TeamIdentifier)' ck_aft_check_signature "$work/aft" test

# A valid ad-hoc signature with identifier aft but no runtime option must fail
# the runtime check, rather than failing because of an identifier mismatch.
cp "$work/original" "$work/aft"
codesign --force --sign - --identifier aft "$work/aft"
reject 'legacy npm signing lacks runtime' 'hardened runtime missing' ck_aft_check_npm_signature "$work/aft" test

for value in true false; do
  cp "$CK_AFT_ENTITLEMENTS" "$work/debug.plist"
  /usr/libexec/PlistBuddy -c "Add :com.apple.security.get-task-allow bool $value" "$work/debug.plist"
  CK_AFT_ENTITLEMENTS="$work/debug.plist" ck_aft_sign_npm "$work/aft"
  reject "get-task-allow=$value is forbidden" 'get-task-allow present' ck_aft_check_npm_signature "$work/aft" test
done

cp "$CK_AFT_ENTITLEMENTS" "$work/extra.plist"
/usr/libexec/PlistBuddy -c 'Add :com.apple.security.cs.allow-jit bool true' "$work/extra.plist"
CK_AFT_ENTITLEMENTS="$work/extra.plist" ck_aft_sign_npm "$work/aft"
reject 'extra runtime exception is forbidden' 'unexpected entitlement' ck_aft_check_npm_signature "$work/aft" test

cp "$CK_AFT_ENTITLEMENTS" "$work/disabled.plist"
/usr/libexec/PlistBuddy -c "Set :$CK_AFT_EXCEPTION false" "$work/disabled.plist"
CK_AFT_ENTITLEMENTS="$work/disabled.plist" ck_aft_sign_npm "$work/aft"
reject 'library validation exception must be true' 'library validation not disabled' ck_aft_check_npm_signature "$work/aft" test

ck_aft_sign_binary "$work/aft" - wrong
reject 'npm identifier must be aft' 'signature identifier is not aft' ck_aft_check_npm_signature "$work/aft" test
codesign --remove-signature "$work/aft"
reject 'unsigned binary is forbidden' 'signature invalid' ck_aft_check_npm_signature "$work/aft" test
echo "signature tests: $passed passed"
