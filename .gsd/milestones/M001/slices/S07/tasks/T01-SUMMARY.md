---
id: T01
parent: S07
milestone: M001
provides:
  - 5 platform npm packages under npm/ with correct os/cpu fields
  - "@aft/core" root package with optionalDependencies on all 5
  - Rewritten resolver with npm-first → PATH → cargo fallback chain
  - Cargo.toml crates.io metadata and release profile optimizations
  - Resolver unit tests covering platform mapping, fallback chain, Windows .exe
key_files:
  - opencode-plugin-aft/src/resolver.ts
  - opencode-plugin-aft/package.json
  - npm/darwin-arm64/package.json
  - npm/win32-x64/package.json
  - opencode-plugin-aft/src/__tests__/resolver.test.ts
  - Cargo.toml
key_decisions:
  - "require.resolve used for npm package lookup — simple try/catch with graceful fallback, no pnp special-casing for v1"
  - "platformKey() takes optional platform/arch params defaulting to process.platform/process.arch — enables pure unit testing without mocks"
  - "Error messages include all attempted resolution sources and why each failed — enables agent self-diagnosis"
patterns_established:
  - "Platform packages use @aft/{os}-{arch} naming, matching esbuild/turbo convention"
  - "Resolver fallback chain: npm package → PATH → cargo, each wrapped in try/catch with reason tracking"
observability_surfaces:
  - "platformKey() exported for direct inspection of platform mapping"
  - "findBinary() error includes attempted sources list with per-source failure reasons"
  - "Unsupported platform errors include exact process.platform and process.arch values"
duration: 25m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T01: Create npm distribution packages and wire resolver

**Created 5 platform npm packages, renamed root to @aft/core with optionalDependencies, rewrote resolver with npm-first fallback chain, added Cargo.toml metadata + release profile, and wrote 13 resolver unit tests.**

## What Happened

Created the npm distribution structure following the esbuild/turbo pattern. Five platform packages under `npm/` each have a `package.json` with `os`/`cpu` arrays, `preferUnplugged: true`, and `bin` pointing to the platform binary (`.exe` suffix for win32). Each has a `bin/.gitkeep` for git tracking — actual binaries placed by CI.

Renamed `opencode-plugin-aft` to `@aft/core` and added `optionalDependencies` on all 5 platform packages at exact `0.1.0` versions. Preserved existing `main`/`types`/`scripts` fields.

Rewrote `resolver.ts` with a three-tier fallback: (1) npm platform package via `require.resolve`, (2) PATH via `which`/`where`, (3) `~/.cargo/bin/aft`. The `platformKey()` function maps `process.platform`/`process.arch` to package names and accepts optional parameters for testability. Error messages include all attempted sources with per-source failure reasons.

Added Cargo.toml metadata (description, license, repository, keywords, categories) and `[profile.release]` with LTO, single codegen unit, strip, and panic=abort. Release binary is 6.4MB (down from ~7.4MB debug).

## Verification

- `bun test opencode-plugin-aft/src/__tests__/resolver.test.ts` — **13 tests pass** (platformKey mapping ×5, unsupported platform ×3, defaults ×1, Windows .exe naming ×2, findBinary integration ×2)
- `bun test opencode-plugin-aft/` — **22 tests pass** across 3 files (bridge, tools, resolver) — zero regressions
- All 5 `npm/*/package.json` verified: correct os/cpu arrays, preferUnplugged=true, version=0.1.0, correct bin paths
- `@aft/core` package.json has 5 optionalDependencies at 0.1.0
- `cargo build --release` succeeds with optimized profile (6.4MB binary)
- `grep -q 'license' Cargo.toml` — PASS

**Slice-level checks (intermediate — T01 of 2):**
- ✅ Resolver unit tests pass
- ⏳ `node scripts/validate-packages.mjs` — T02 artifact, not yet created
- ⏳ `actionlint .github/workflows/release.yml` — T02 artifact
- ✅ `cargo build --release` passes
- ✅ Cargo.toml license field present
- ✅ Resolver error on unsupported platform includes platform+arch values

## Diagnostics

- Call `platformKey("darwin", "arm64")` directly to verify platform mapping without running the full resolver
- `findBinary()` errors include an "Attempted sources:" section listing each source and why it failed
- Platform packages are inspectable via `jq '.os, .cpu' npm/*/package.json`

## Deviations

None.

## Known Issues

- `require.resolve` in the npm package lookup may not work in all pnpm strict-mode configurations. Simple try/catch handles this gracefully (falls back to PATH/cargo). Special pnp handling deferred to a future iteration if needed.

## Files Created/Modified

- `npm/darwin-arm64/package.json` — platform package for macOS ARM64
- `npm/darwin-arm64/bin/.gitkeep` — placeholder for CI-placed binary
- `npm/darwin-x64/package.json` — platform package for macOS Intel
- `npm/darwin-x64/bin/.gitkeep` — placeholder for CI-placed binary
- `npm/linux-arm64/package.json` — platform package for Linux ARM64
- `npm/linux-arm64/bin/.gitkeep` — placeholder for CI-placed binary
- `npm/linux-x64/package.json` — platform package for Linux x64
- `npm/linux-x64/bin/.gitkeep` — placeholder for CI-placed binary
- `npm/win32-x64/package.json` — platform package for Windows x64
- `npm/win32-x64/bin/.gitkeep` — placeholder for CI-placed binary
- `opencode-plugin-aft/package.json` — renamed to @aft/core, added optionalDependencies
- `opencode-plugin-aft/src/resolver.ts` — rewritten with npm-first resolution, exported platformKey()
- `opencode-plugin-aft/src/__tests__/resolver.test.ts` — 13 new resolver unit tests
- `Cargo.toml` — added crates.io metadata and [profile.release] optimizations
- `.gsd/milestones/M001/slices/S07/S07-PLAN.md` — added Observability/Diagnostics section, updated Verification
- `.gsd/milestones/M001/slices/S07/tasks/T01-PLAN.md` — added Observability Impact section
