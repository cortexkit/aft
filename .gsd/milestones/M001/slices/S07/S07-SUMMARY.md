---
id: S07
parent: M001
milestone: M001
provides:
  - 5 npm platform packages (@aft/darwin-arm64, darwin-x64, linux-arm64, linux-x64, win32-x64) with correct os/cpu fields and bin paths
  - "@aft/core" root package with optionalDependencies on all 5 platform packages
  - Binary resolver with npm-first → PATH → cargo fallback chain and diagnostic error messages
  - GitHub Actions release workflow — 5-platform cross-compilation on v* tag push, ordered npm publish
  - Version sync script updating 7 files (6 package.json + Cargo.toml) from git tag
  - Package validation script for structural health checking across all 6 packages
  - Cargo.toml crates.io metadata and release profile optimizations (LTO, strip, codegen-units=1)
requires:
  - slice: S06
    provides: Plugin code (opencode-plugin-aft/), binary resolver interface, JSON protocol contract
  - slice: S01
    provides: Cargo.toml build config
key_files:
  - opencode-plugin-aft/src/resolver.ts
  - opencode-plugin-aft/package.json
  - npm/darwin-arm64/package.json
  - npm/win32-x64/package.json
  - opencode-plugin-aft/src/__tests__/resolver.test.ts
  - Cargo.toml
  - .github/workflows/release.yml
  - scripts/version-sync.mjs
  - scripts/validate-packages.mjs
key_decisions:
  - "D035: @aft/{os}-{arch} platform package naming following esbuild/turbo convention"
  - "D036: Resolver fallback chain order — npm package → PATH → cargo, each with reason tracking"
  - "D037: platformKey() accepts optional params for pure unit testing without process globals"
  - "D038: Separate CI build jobs per platform (not matrix) — each has distinct tooling"
  - "D039: cross v0.2.5 for Linux musl builds — known-stable per ripgrep CI patterns"
  - "D040: Version source of truth is git tag — CI derives version via GITHUB_REF_NAME"
patterns_established:
  - "Platform packages use @aft/{os}-{arch} naming with os/cpu fields matching process.platform/process.arch"
  - "Resolver fallback chain: npm package → PATH → cargo, each wrapped in try/catch with reason tracking"
  - "Version sync from git tag — single script updates all 7 version locations atomically"
  - "validate-packages.mjs as structural health check — single command confirms all packages are publish-ready"
observability_surfaces:
  - "platformKey() exported for direct inspection of platform mapping"
  - "findBinary() error includes attempted sources list with per-source failure reasons"
  - "Unsupported platform errors include exact process.platform and process.arch values"
  - "node scripts/validate-packages.mjs — exit 0 with summary or exit 1 with per-check failure messages"
  - "node scripts/version-sync.mjs <version> --dry-run — preview all update targets without writing"
drill_down_paths:
  - .gsd/milestones/M001/slices/S07/tasks/T01-SUMMARY.md
  - .gsd/milestones/M001/slices/S07/tasks/T02-SUMMARY.md
duration: 45m
verification_result: passed
completed_at: 2026-03-14
---

# S07: Binary Distribution Pipeline

**Complete npm distribution infrastructure — 5 platform packages, CI cross-compilation for all targets, version sync, and package validation tooling.**

## What Happened

Built the full binary distribution pipeline in two tasks. T01 created the npm package structure following the esbuild/turbo pattern: 5 platform packages under `npm/` with correct `os`/`cpu` fields, each pointing to a `bin/aft` (or `bin/aft.exe` for Windows). Renamed the plugin package to `@aft/core` with `optionalDependencies` on all 5 platforms. Rewrote the binary resolver with a three-tier fallback (npm platform package via `require.resolve` → PATH → `~/.cargo/bin/aft`) and diagnostic errors showing all attempted sources. Added Cargo.toml crates.io metadata and release profile optimizations (LTO, single codegen unit, strip, panic=abort) — release binary dropped from ~7.4MB to 6.4MB.

T02 built the CI automation: a GitHub Actions workflow triggered on `v*` tag push runs 5 parallel build jobs (macOS ARM64 native, macOS x64 cross-target, Linux ARM64 + x64 via `cross` with musl, Windows x64 MSVC), then a publish job downloads all artifacts into platform package dirs and publishes in correct order (platform packages first, then `@aft/core`). A version sync script updates all 7 version locations from the git tag. A package validation script provides a single-command structural health check for all 6 packages.

## Verification

- **Resolver tests:** 13/13 pass — platform mapping ×5, unsupported platform ×3, defaults ×1, Windows .exe ×2, findBinary integration ×2
- **Full plugin test suite:** 22/22 pass across 3 files (bridge, tools, resolver) — zero regressions
- **Package validation:** `node scripts/validate-packages.mjs` passes — all 6 packages have correct structure, os/cpu fields, version alignment at 0.1.0
- **Version sync:** `node scripts/version-sync.mjs 0.2.0 --dry-run` shows 12 updates across 7 files correctly
- **Release build:** `cargo build --release` succeeds with optimized profile
- **Cargo.toml metadata:** license, description, repository, keywords all present
- **CI workflow:** 6 jobs (5 build + 1 publish), publish depends on all build jobs, correct cross/native tooling per platform

## Requirements Advanced

- R012 (Binary distribution) — fully implemented: npm platform packages, CI pipeline, resolver with fallback chain, cargo install metadata

## Requirements Validated

- R012 — 5 platform packages with correct os/cpu fields verified by validate-packages.mjs, resolver unit tests prove platform mapping and fallback chain, CI workflow defines complete build→publish pipeline for all 5 targets, Cargo.toml has crates.io metadata for `cargo install aft` fallback

## New Requirements Surfaced

- none

## Requirements Invalidated or Re-scoped

- none

## Deviations

None.

## Known Limitations

- `require.resolve` in the npm package lookup may not work in all pnpm strict-mode configurations. Handled gracefully (falls back to PATH/cargo). Special pnp handling deferred.
- CI workflow uses separate build jobs instead of matrix strategy — intentional, each platform has distinct tooling needs.
- Actual binary distribution requires npm registry access and a `v*` git tag push. Local validation substitutes for CI verification.

## Follow-ups

- none — this is the terminal slice of M001

## Files Created/Modified

- `npm/darwin-arm64/package.json` — platform package for macOS ARM64
- `npm/darwin-arm64/bin/.gitkeep` — placeholder for CI-placed binary
- `npm/darwin-x64/package.json` — platform package for macOS Intel
- `npm/darwin-x64/bin/.gitkeep` — placeholder
- `npm/linux-arm64/package.json` — platform package for Linux ARM64
- `npm/linux-arm64/bin/.gitkeep` — placeholder
- `npm/linux-x64/package.json` — platform package for Linux x64
- `npm/linux-x64/bin/.gitkeep` — placeholder
- `npm/win32-x64/package.json` — platform package for Windows x64
- `npm/win32-x64/bin/.gitkeep` — placeholder
- `opencode-plugin-aft/package.json` — renamed to @aft/core, added optionalDependencies
- `opencode-plugin-aft/src/resolver.ts` — rewritten with npm-first resolution chain
- `opencode-plugin-aft/src/__tests__/resolver.test.ts` — 13 resolver unit tests
- `Cargo.toml` — crates.io metadata and release profile optimizations
- `.github/workflows/release.yml` — 6-job release pipeline (5 build + 1 publish)
- `scripts/version-sync.mjs` — version sync across 7 files from git tag
- `scripts/validate-packages.mjs` — structural health check for all 6 packages

## Forward Intelligence

### What the next slice should know
- M001 is complete after S07. The next work is M002 or milestone-level UAT.
- All 11 commands are registered as OpenCode tools. 142 Rust tests + 22 plugin tests pass.
- The release workflow is untested in CI — first real `v*` tag push will be the proof. Package structure is validated locally.

### What's fragile
- `require.resolve` for npm package lookup — pnpm strict mode or Yarn PnP may not resolve `@aft/{platform}/bin/aft`. The try/catch fallback handles this, but the npm-first path is untested in those package managers.
- CI workflow depends on `cross` v0.2.5 for Linux musl builds — cross version pinning may need updating if Rust toolchain changes.

### Authoritative diagnostics
- `node scripts/validate-packages.mjs` — single-command check that all 6 packages are structurally correct. If this passes, package configuration is sound.
- `findBinary()` errors list every attempted source and why each failed — the error message itself is the diagnostic.

### What assumptions changed
- No assumptions changed — this was a straightforward assembly slice with no surprises.
