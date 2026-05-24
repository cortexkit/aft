# S07: Binary Distribution Pipeline — UAT

**Milestone:** M001
**Written:** 2026-03-14

## UAT Type

- UAT mode: artifact-driven
- Why this mode is sufficient: S07 produces package configuration, CI workflow YAML, and scripts — not runtime features. All artifacts can be validated by inspecting structure, running scripts, and verifying resolver logic via unit tests. Actual CI execution and npm publish require external infrastructure not available locally.

## Preconditions

- Repository checked out with S07 changes applied
- Node.js and Bun installed (for script execution and tests)
- Rust toolchain installed (for `cargo build --release`)

## Smoke Test

Run `node scripts/validate-packages.mjs` — should exit 0 with "All 6 packages validated successfully."

## Test Cases

### 1. Platform package structure

1. Inspect each of the 5 directories: `npm/darwin-arm64/`, `npm/darwin-x64/`, `npm/linux-arm64/`, `npm/linux-x64/`, `npm/win32-x64/`
2. Each must have a `package.json` and a `bin/.gitkeep`
3. Run `jq '.os, .cpu, .preferUnplugged' npm/darwin-arm64/package.json`
4. **Expected:** `os: ["darwin"]`, `cpu: ["arm64"]`, `preferUnplugged: true`
5. Run `jq '.bin' npm/win32-x64/package.json`
6. **Expected:** bin field points to `bin/aft.exe` (not `bin/aft`)

### 2. Root package optionalDependencies

1. Run `jq '.optionalDependencies' opencode-plugin-aft/package.json`
2. **Expected:** Contains all 5 platform packages (`@aft/darwin-arm64`, `@aft/darwin-x64`, `@aft/linux-arm64`, `@aft/linux-x64`, `@aft/win32-x64`) at version `0.1.0`
3. Verify `name` field is `@aft/core`

### 3. Resolver platform mapping

1. Run `bun test opencode-plugin-aft/src/__tests__/resolver.test.ts`
2. **Expected:** 13 tests pass, 0 fail
3. Verify test output includes: darwin+arm64 → darwin-arm64, darwin+x64 → darwin-x64, linux+arm64 → linux-arm64, linux+x64 → linux-x64, win32+x64 → win32-x64

### 4. Resolver unsupported platform error

1. In the test output, verify the "unsupported platform" tests
2. **Expected:** Error messages include the specific `process.platform` and `process.arch` values (e.g., "freebsd" and "arm64" appear in the error string)

### 5. Resolver fallback chain

1. In the test output, verify the `findBinary` tests
2. **Expected:** `findBinary` either finds the binary via PATH/cargo (if `aft` is built locally) or throws an error listing all attempted sources with per-source failure reasons
3. The error should mention at least: npm package lookup, PATH search, and cargo bin check

### 6. Version sync dry run

1. Run `node scripts/version-sync.mjs 0.3.0 --dry-run`
2. **Expected:** Output shows 12 updates across 7 files: 5 platform package versions, core package version + 5 optionalDependency versions, Cargo.toml version
3. Run `jq '.version' npm/darwin-arm64/package.json` — should still be `0.1.0` (dry run doesn't modify)

### 7. Version sync actual

1. Run `node scripts/version-sync.mjs 0.2.0` (non-dry-run)
2. Run `jq '.version' npm/darwin-arm64/package.json`
3. **Expected:** `0.2.0`
4. Run `jq '.optionalDependencies["@aft/darwin-arm64"]' opencode-plugin-aft/package.json`
5. **Expected:** `0.2.0`
6. Run `grep '^version' Cargo.toml`
7. **Expected:** `version = "0.2.0"`
8. **Cleanup:** Run `node scripts/version-sync.mjs 0.1.0` to restore original version

### 8. Package validation

1. Run `node scripts/validate-packages.mjs`
2. **Expected:** Exit code 0, output includes "All 6 packages validated successfully", version alignment, correct os/cpu fields, preferUnplugged set, optionalDependencies complete

### 9. Release binary build

1. Run `cargo build --release`
2. **Expected:** Compiles successfully with `release` profile (optimized)
3. Check binary size: `ls -lh target/release/aft`
4. **Expected:** Binary exists, size under 10MB (LTO + strip optimizations applied)

### 10. Cargo.toml metadata

1. Run `grep 'license' Cargo.toml`
2. **Expected:** `license = "MIT"` present
3. Run `grep 'description' Cargo.toml`
4. **Expected:** description field present with non-empty value
5. Run `grep 'repository' Cargo.toml`
6. **Expected:** repository URL present

### 11. CI workflow structure

1. Open `.github/workflows/release.yml`
2. Verify trigger: `on: push: tags: ['v*']`
3. Count build jobs — should be 5: darwin-arm64, darwin-x64, linux-arm64, linux-x64, win32-x64
4. Verify publish job has `needs:` listing all 5 build jobs
5. **Expected:** Publish job downloads all 5 artifacts, runs version-sync and validate-packages, publishes platform packages before @aft/core

### 12. No regressions in plugin tests

1. Run `bun test opencode-plugin-aft/`
2. **Expected:** 22 tests pass across 3 files (bridge, tools, resolver), 0 failures

## Edge Cases

### Unsupported platform/arch combinations

1. In resolver test output, verify that `win32 + arm64`, `freebsd + x64`, and `linux + mips` all throw with descriptive errors
2. **Expected:** Each error includes the specific platform and arch values that were attempted

### Windows binary naming

1. In resolver test output, verify that win32-x64 lookups expect `.exe` suffix
2. Verify non-win32 platforms do NOT append `.exe`
3. **Expected:** Binary names are `aft.exe` on Windows and `aft` everywhere else

### Version sync with invalid version

1. Run `node scripts/version-sync.mjs not-a-version`
2. **Expected:** Script rejects with a semver validation error (does not write garbage versions)

## Failure Signals

- `node scripts/validate-packages.mjs` exits non-zero — package structure is misconfigured
- Resolver tests fail — platform mapping or fallback chain is broken
- `cargo build --release` fails — Cargo.toml or release profile is misconfigured
- Version sync produces wrong number of updates (should be 12 across 7 files)
- CI workflow YAML has syntax errors or missing job dependencies
- Plugin test regressions — S07 changes broke existing bridge or tool tests

## Requirements Proved By This UAT

- R012 (Binary distribution) — npm platform packages exist with correct structure, resolver maps platforms correctly with fallback chain, CI workflow defines cross-compilation for all 5 targets, Cargo.toml has metadata for `cargo install aft` fallback

## Not Proven By This UAT

- Actual CI execution (requires GitHub Actions runners and a v* tag push)
- Actual npm publish (requires npm registry access and authentication)
- Real `npm install @aft/core` on each platform (requires platform-specific CI runners)
- Cross-compilation producing working binaries on non-native platforms (requires CI runners)

## Notes for Tester

- Test case 7 (version sync actual) modifies files — run the cleanup step afterward to restore 0.1.0
- The CI workflow is validated by structural inspection only. First real execution happens on the first `v*` tag push.
- Platform packages contain `bin/.gitkeep` placeholders — actual binaries are placed by CI at publish time
- If running on a machine where `aft` has been built locally, `findBinary` test may find it via PATH. This is correct behavior — the test verifies the fallback chain works.
