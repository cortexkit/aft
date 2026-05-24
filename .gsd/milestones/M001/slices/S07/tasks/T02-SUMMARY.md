---
id: T02
parent: S07
milestone: M001
provides:
  - GitHub Actions release workflow for 5-platform cross-compilation and npm publish
  - Version sync script updating 7 locations from a single version string
  - Package validation script for structural health checking of all 6 packages
key_files:
  - .github/workflows/release.yml
  - scripts/version-sync.mjs
  - scripts/validate-packages.mjs
key_decisions:
  - "Separate build jobs (not matrix) for each platform — clearer per-platform config, easier to debug failures"
  - "cross pinned to v0.2.5 for Linux musl builds — known-stable version per ripgrep CI patterns"
  - "Platform packages published before @aft/core with --access public — prevents install-time race on scoped packages"
  - "Version sync uses regex replacement for Cargo.toml — avoids TOML parser dependency for a single field update"
patterns_established:
  - "Version source of truth is the git tag — CI derives version via GITHUB_REF_NAME, version-sync propagates to all 7 files"
  - "validate-packages.mjs is the structural health check — run before publish to catch misconfigurations"
observability_surfaces:
  - "node scripts/validate-packages.mjs — exit 0 with summary or exit 1 with per-check failure messages"
  - "node scripts/version-sync.mjs <version> --dry-run — shows all update targets without writing"
  - "CI artifact names match platform directory names (darwin-arm64, etc.) — download-artifact step maps 1:1"
duration: 20m
verification_result: passed
completed_at: 2026-03-14
blocker_discovered: false
---

# T02: Add CI release workflow and version sync

**Built GitHub Actions release workflow with 5 parallel cross-compilation jobs, npm publish pipeline, version sync script for 7 locations, and package validation script.**

## What Happened

Created `.github/workflows/release.yml` triggered on `v*` tag push. Five separate build jobs run in parallel: macOS ARM64 (native on `macos-latest`), macOS x64 (cross-target on `macos-latest`), Linux ARM64 and x64 (both using `cross` v0.2.5 with musl for static binaries on `ubuntu-latest`), and Windows x64 (native MSVC on `windows-latest`). Each job uses `dtolnay/rust-toolchain@stable`, builds the release binary, and uploads it as a named artifact matching the platform directory name.

A `publish` job depends on all 5 build jobs. It downloads all artifacts into the correct `npm/{platform}/bin/` directories, sets binary permissions, runs `version-sync.mjs --from-tag` to derive version from the git tag, validates packages, then publishes all 5 platform packages (with `--access public` for scoped packages) before publishing `@aft/core` last.

Created `scripts/version-sync.mjs` that takes a version string (or `--from-tag` to read `GITHUB_REF_NAME`) and updates all 7 files: 5 platform package.json versions, the core package.json version + all 5 optionalDependencies versions, and Cargo.toml version. Supports `--dry-run` to preview changes. Validates semver format.

Created `scripts/validate-packages.mjs` that checks all 6 package.json files for: correct name, os/cpu arrays matching directory, preferUnplugged, bin field, optionalDependencies completeness in core, and version alignment across all packages.

## Verification

- `node scripts/validate-packages.mjs` — **PASS**: all 6 packages validated, versions aligned at 0.1.0
- `node scripts/version-sync.mjs 0.2.0 --dry-run` — **PASS**: shows 12 updates across 7 files (5 platform versions, core version + 5 optDeps versions, Cargo.toml version)
- Workflow YAML parses correctly, has 6 jobs (5 build + 1 publish), publish depends on all 5 build jobs
- `cargo build --release` — **PASS**: compiles with LTO and strip
- `bun test opencode-plugin-aft/src/__tests__/resolver.test.ts` — **13 pass, 0 fail** (no regressions)

**Slice-level checks (final task — all must pass):**
- ✅ Resolver unit tests pass (13/13)
- ✅ `node scripts/validate-packages.mjs` — passes
- ✅ Workflow YAML has 5 build targets, correct cross/native tooling, publish ordering
- ✅ `cargo build --release` — compiles successfully
- ✅ Cargo.toml license field present (`license = "MIT"`)
- ✅ Resolver error on unsupported platform includes platform+arch values (verified via unit test)

## Diagnostics

- `node scripts/validate-packages.mjs` — single-command structural health check. Exit 0 = all good; exit 1 prints which checks failed
- `node scripts/version-sync.mjs <version> --dry-run` — preview version sync targets without writing
- CI job artifact names match platform dirs (darwin-arm64, linux-x64, etc.) — failed builds are identifiable by artifact name
- Workflow uses `if-no-files-found: error` on artifact upload — missing binary immediately fails the build job

## Deviations

None.

## Known Issues

- Workflow uses separate build jobs instead of a matrix strategy. This is intentional — each platform has different tooling (native cargo vs cross, strip vs no-strip, .exe vs no .exe), making a matrix less clean than explicit jobs.
- `grep -q '"license"' Cargo.toml` in the slice plan fails because TOML doesn't quote keys. The field exists (`license = "MIT"`). Fixed verification to use `grep -q 'license' Cargo.toml`.

## Files Created/Modified

- `.github/workflows/release.yml` — complete 6-job release pipeline (5 build + 1 publish)
- `scripts/version-sync.mjs` — version synchronization across 7 files
- `scripts/validate-packages.mjs` — package structure validation for all 6 packages
- `.gsd/milestones/M001/slices/S07/tasks/T02-PLAN.md` — added Observability Impact section (pre-flight fix)
