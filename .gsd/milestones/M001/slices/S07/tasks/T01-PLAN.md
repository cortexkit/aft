---
estimated_steps: 7
estimated_files: 9
---

# T01: Create npm distribution packages and wire resolver

**Slice:** S07 — Binary Distribution Pipeline
**Milestone:** M001

## Description

Create the npm distribution structure that makes `npm install @aft/core` deliver the correct platform binary. Five platform packages under `npm/` each contain a `package.json` with `os`/`cpu` fields that npm uses for platform filtering. The root `@aft/core` package (evolved from `opencode-plugin-aft`) declares `optionalDependencies` on all five. The resolver gets rewritten to check the npm platform package first (highest priority), falling back to PATH and cargo. Cargo.toml gets crates.io metadata and release profile optimizations.

## Steps

1. Create `npm/darwin-arm64/package.json`, `npm/darwin-x64/package.json`, `npm/linux-arm64/package.json`, `npm/linux-x64/package.json`, `npm/win32-x64/package.json` — each with: name (`@aft/{platform}`), version `0.1.0`, `os` array, `cpu` array, `preferUnplugged: true`, `bin` field pointing to `bin/aft` (or `bin/aft.exe` for win32). Add `.gitkeep` in each `bin/` dir so git tracks the structure (actual binaries placed by CI).

2. Update `opencode-plugin-aft/package.json`: rename to `@aft/core`, add `optionalDependencies` for all 5 platform packages at `0.1.0`, add `bin` stub entry (optional CLI shim), ensure `main`/`types` fields stay correct for plugin usage.

3. Rewrite `opencode-plugin-aft/src/resolver.ts`: new resolution order is (a) npm platform package via `require.resolve(`@aft/${platformKey}/bin/aft${ext}`)` in try/catch, using a `platformKey()` function that maps `process.platform`/`process.arch` to package names, (b) PATH via `which aft` (existing), (c) `~/.cargo/bin/aft` (existing). Handle `.exe` suffix when `process.platform === 'win32'`. Export the `platformKey` helper for testability.

4. Add `Cargo.toml` metadata: `description`, `license = "MIT"`, `repository`, `keywords`, `categories`. Add `[profile.release]` with `lto = true`, `codegen-units = 1`, `strip = true`, `panic = "abort"`.

5. Write `opencode-plugin-aft/src/__tests__/resolver.test.ts`: unit tests for `platformKey()` mapping (darwin+arm64 → darwin-arm64, linux+x64 → linux-x64, win32+x64 → win32-x64, unsupported platform → throws). Integration test that `findBinary()` finds the debug binary via PATH/cargo fallback (existing behavior still works). Test for Windows `.exe` suffix logic via the platform key helper.

6. Run `bun test` to verify resolver tests pass and existing bridge/tools tests still pass.

7. Verify all package.json files: correct os/cpu fields, version alignment at 0.1.0, optionalDependencies in @aft/core match platform package names.

## Must-Haves

- [ ] 5 platform package.json files with correct `os`/`cpu` arrays and `preferUnplugged: true`
- [ ] `@aft/core` package.json with `optionalDependencies` on all 5 platform packages
- [ ] Resolver checks npm platform package first, then PATH, then cargo
- [ ] Resolver handles `.exe` suffix for Windows
- [ ] `platformKey()` exported and unit-tested
- [ ] Cargo.toml has description, license, repository, and `[profile.release]` optimizations
- [ ] Existing bridge and tools tests still pass

## Verification

- `bun test opencode-plugin-aft/src/__tests__/resolver.test.ts` — all resolver tests pass
- `bun test opencode-plugin-aft/` — all existing tests still pass (no regressions)
- All 5 `npm/*/package.json` files exist with correct structure
- `@aft/core` package.json has 5 optionalDependencies
- `cargo build --release` succeeds with new profile settings

## Observability Impact

- **`platformKey()` export** — Future agents can import and call `platformKey()` directly to verify platform mapping without running the resolver. Returns `"darwin-arm64"` / `"linux-x64"` / etc., or throws with `process.platform` + `process.arch` in the error.
- **Resolver error messages** — `findBinary()` now reports which resolution sources were attempted (npm package, PATH, cargo) and why each failed. Unsupported platform errors include the exact `process.platform`/`process.arch` values.
- **Package structure** — 5 `npm/*/package.json` files are inspectable with `cat` or JSON tools. Each has `os`/`cpu` arrays that can be validated programmatically.
- **Failure path test** — Unit test verifies that unsupported platform throws with platform+arch in the message.

## Inputs

- `opencode-plugin-aft/src/resolver.ts` — existing resolver with clear slot for npm platform package resolution
- `opencode-plugin-aft/package.json` — current package named `opencode-plugin-aft`
- `Cargo.toml` — minimal, needs metadata additions
- S07-RESEARCH.md — esbuild/turbo pattern details, platform package structure, resolver approach

## Expected Output

- `npm/darwin-arm64/package.json` — platform package for macOS ARM
- `npm/darwin-x64/package.json` — platform package for macOS Intel
- `npm/linux-arm64/package.json` — platform package for Linux ARM
- `npm/linux-x64/package.json` — platform package for Linux x64
- `npm/win32-x64/package.json` — platform package for Windows x64
- `opencode-plugin-aft/package.json` — renamed to `@aft/core` with optionalDependencies
- `opencode-plugin-aft/src/resolver.ts` — rewritten with npm-first resolution
- `opencode-plugin-aft/src/__tests__/resolver.test.ts` — new resolver unit tests
- `Cargo.toml` — crates.io metadata + release profile
