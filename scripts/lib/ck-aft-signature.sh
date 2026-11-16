#!/usr/bin/env bash
# macOS signing, signature checks and the ONNX Runtime smoke for the
# supervised AFT module (ck-aft) and npm binaries. Staging and placement require
# a team signature; npm uses an ad-hoc signature with the stable identifier aft.
#
# The rule: identifier ck-aft, signed by a team identity (macOS permission
# grants given to the module, such as Files & Folders access for its own read
# and search tools, are recorded against identifier plus team, so neither may
# change between cards, and an ad-hoc card would be asked again after every
# rebuild), hardened runtime, no get-task-allow, and exactly one
# hardened-runtime exception, com.apple.security.cs.disable-library-validation.
# AFT needs it to load the ONNX Runtime library downloaded from Microsoft's
# releases, which is signed without a Team ID and would otherwise be refused.
#
# The signing identity comes from CK_AFT_SIGN_IDENTITY, else the first valid
# "Apple Development:" identity in the login keychain. It is never committed.

CK_AFT_IDENTIFIER="ck-aft"
CK_AFT_ENTITLEMENTS="${CK_AFT_ENTITLEMENTS:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/ck-aft.entitlements.plist}"
CK_AFT_EXCEPTION="com.apple.security.cs.disable-library-validation"

# Resolve the team signing identity, or fail with the reason.
ck_aft_sign_identity() {
  if [ -n "${CK_AFT_SIGN_IDENTITY:-}" ]; then
    printf '%s\n' "$CK_AFT_SIGN_IDENTITY"
    return 0
  fi
  local found
  found="$(security find-identity -v -p codesigning 2>/dev/null |
    sed -n 's/^ *[0-9][0-9]*) [0-9A-F]\{40\} "\(Apple Development: [^"]*\)"$/\1/p' | head -1)"
  if [ -z "$found" ]; then
    echo "ck-aft signing: no Apple Development identity in the keychain; set CK_AFT_SIGN_IDENTITY" >&2
    return 1
  fi
  printf '%s\n' "$found"
}

# Both distribution paths use the same hardened runtime and entitlements.
ck_aft_sign_binary() {
  local binary="$1" identity="$2" identifier="$3"
  codesign --force --sign "$identity" --identifier "$identifier" -o runtime \
    --entitlements "$CK_AFT_ENTITLEMENTS" "$binary"
}

# Sign daemon cards with their pinned identifier and team identity.
ck_aft_sign() {
  local identity
  identity="$(ck_aft_sign_identity)" || return 1
  ck_aft_sign_binary "$1" "$identity" "$CK_AFT_IDENTIFIER"
}

# npm has no team identity. Pin aft instead of codesign's content-derived ID.
ck_aft_sign_npm() {
  ck_aft_sign_binary "$1" - aft
}

# Read back the signed bytes, including all entitlements, before publication.
# Usage: ck_aft_check_binary_signature <binary> <error-prefix> <identifier> <require-team>
ck_aft_check_binary_signature() {
  local binary="$1" prefix="$2" identifier="$3" require_team="$4"
  local details entitlements_xml flags plist keys key team

  if ! codesign --verify --strict "$binary" 2>/dev/null; then
    echo "$prefix: signature invalid: codesign --verify --strict failed for $binary" >&2
    return 1
  fi
  details="$(codesign -dv "$binary" 2>&1)" || {
    echo "$prefix: signature unreadable: codesign -dv failed for $binary" >&2
    return 1
  }
  printf '%s\n' "$details" | sed 's/^/    /'

  if ! printf '%s\n' "$details" | grep -qx "Identifier=$identifier"; then
    echo "$prefix: signature identifier is not $identifier" >&2
    return 1
  fi
  team="$(printf '%s\n' "$details" | sed -n 's/^TeamIdentifier=//p')"
  if [ "$require_team" = true ] && { [ -z "$team" ] || [ "$team" = "not set" ]; }; then
    echo "$prefix: signed ad hoc (no TeamIdentifier): macOS would ask for the module's permissions again after every card" >&2
    return 1
  fi
  flags="$(printf '%s\n' "$details" | sed -n 's/^CodeDirectory .*flags=0x[0-9a-f]*(\([^)]*\)).*/\1/p')"
  case ",$flags," in
    *,runtime,*) ;;
    *)
      echo "$prefix: hardened runtime missing: CodeDirectory flags are ($flags), expected runtime" >&2
      return 1
      ;;
  esac

  entitlements_xml="$(codesign -d --entitlements - --xml "$binary" 2>/dev/null)"
  echo "    entitlements: ${entitlements_xml:-<none>}"
  plist="$(mktemp "${TMPDIR:-/tmp}/ck-aft-entitlements.XXXXXX")"
  printf '%s' "$entitlements_xml" > "$plist"
  # PlistBuddy, not plutil: the keys must be read literally, and plutil splits
  # a key on its dots as a key path.
  keys="$(/usr/libexec/PlistBuddy -c Print "$plist" 2>/dev/null | sed -n 's/^    \([^ ]*\) = .*/\1/p')"
  if /usr/libexec/PlistBuddy -c "Print :com.apple.security.get-task-allow" "$plist" >/dev/null 2>&1; then
    rm -f "$plist"
    echo "$prefix: get-task-allow present: any same-user process could attach a debugger" >&2
    return 1
  fi
  if [ "$(/usr/libexec/PlistBuddy -c "Print :$CK_AFT_EXCEPTION" "$plist" 2>/dev/null)" != "true" ]; then
    rm -f "$plist"
    echo "$prefix: library validation not disabled: $CK_AFT_EXCEPTION is not true, so the ONNX Runtime load would fail" >&2
    return 1
  fi
  rm -f "$plist"
  for key in $keys; do
    if [ "$key" != "$CK_AFT_EXCEPTION" ]; then
      echo "$prefix: unexpected entitlement $key: the module carries only $CK_AFT_EXCEPTION" >&2
      return 1
    fi
  done
  echo "    signature ok: identifier $identifier, team $team, flags ($flags), exceptions: $CK_AFT_EXCEPTION"
}

# Daemon permissions are keyed to identifier plus team, so ad hoc is forbidden.
ck_aft_check_signature() {
  ck_aft_check_binary_signature "$1" "$2" "$CK_AFT_IDENTIFIER" true
}

# npm permits ad hoc, but never relaxes the runtime or entitlement checks.
ck_aft_check_npm_signature() {
  ck_aft_check_binary_signature "$1" "$2" aft false
}

# Run the one code path the exception exists for: a real ONNX Runtime load.
# A flags-only check would pass a binary whose semantic search is dead.
# Runs `warmup --only semantic` on a one-file project with an isolated HOME and
# storage, pointing ORT_DYLIB_PATH at an installed runtime.
# Usage: ck_aft_smoke_onnx <binary> <error-prefix>
ck_aft_smoke_onnx() {
  local binary="$1" prefix="$2"
  local ort="${CK_SMOKE_ORT_DYLIB:-$HOME/.local/share/cortexkit/aft/onnxruntime/1.24.4/libonnxruntime.dylib}"
  local models="${CK_SMOKE_MODEL_CACHE:-$HOME/.local/share/cortexkit/aft/semantic/models}"
  local work output status

  if [ ! -f "$ort" ]; then
    echo "$prefix: ONNX Runtime smoke cannot run: no runtime at $ort (set CK_SMOKE_ORT_DYLIB)" >&2
    return 1
  fi
  if [ ! -d "$models/models--Qdrant--all-MiniLM-L6-v2-onnx" ]; then
    echo "$prefix: ONNX Runtime smoke cannot run: no MiniLM model cache in $models (set CK_SMOKE_MODEL_CACHE)" >&2
    return 1
  fi
  work="$(mktemp -d "${TMPDIR:-/tmp}/ck-aft-smoke.XXXXXX")"
  mkdir -p "$work/home" "$work/storage/semantic" "$work/root"
  printf 'export function greetUser(name: string) {\n  return `hello ${name}`;\n}\n' > "$work/root/app.ts"
  # A copy, so the smoke never writes into the operator's model cache; `-c`
  # clones on APFS and costs no space.
  cp -Rc "$models" "$work/storage/semantic/models" 2>/dev/null || cp -R "$models" "$work/storage/semantic/models"
  # Keep test-launched executables visibly separate from fleet daemons in
  # Activity Monitor, even when the signed candidate itself is named ck-aft.
  cp "$binary" "$work/ckdev-aft"
  chmod 755 "$work/ckdev-aft"

  status=0
  output="$(env -i PATH=/usr/bin:/bin HOME="$work/home" AFT_STORAGE_DIR="$work/storage" \
    ORT_DYLIB_PATH="$ort" "$work/ckdev-aft" warmup --root "$work/root" --only semantic --timeout 120000 2>&1)" || status=$?
  rm -rf "$work"
  if [ "$status" -ne 0 ] || ! printf '%s\n' "$output" | grep -q '^aft warmup: semantic_index ready$'; then
    printf '%s\n' "$output" | grep -E 'semantic_index|warmup failed|ONNX|onnx' | sed 's/^/    /' >&2
    echo "$prefix: ONNX Runtime smoke failed: the signed binary could not load $ort (exit $status)" >&2
    return 1
  fi
  echo "    smoke: exercised $CK_AFT_EXCEPTION: loaded ONNX Runtime $ort (sha256 $(shasum -a 256 "$ort" | awk '{print $1}')) and built a semantic index"
}
