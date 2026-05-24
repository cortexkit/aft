# S07: Binary Distribution Pipeline

**Goal:** `npm install @aft/core` installs the correct platform binary on macOS ARM/Intel, Linux ARM/x64, and Windows x64. `cargo install aft` works as fallback. CI cross-compiles and publishes all packages on git tag push.

**Demo:** All 6 npm packages have valid structure (5 platform + 1 root). Resolver correctly maps `process.platform`/`process.arch` to the right platform package and finds the binary. CI workflow defines matrix builds for all 5 targets with correct cross-compilation tooling. Release binary builds locally with size optimizations.

## Must-Haves

- 5 platform npm packages (`@aft/darwin-arm64`, `@aft/darwin-x64`, `@aft/linux-arm64`, `@aft/linux-x64`, `@aft/win32-x64`) with correct `os`/`cpu` fields and `preferUnplugged: true`
- `@aft/core` root package with `optionalDependencies` on all 5 platform packages, containing plugin code + resolver
- Resolver checks npm platform package first (via `require.resolve`), then PATH, then `~/.cargo/bin/aft` — with graceful fallback on each step
- Resolver handles Windows binary naming (`.exe` suffix)
- GitHub Actions workflow cross-compiles for all 5 targets using `cross` for Linux (musl), native for macOS and Windows
- Workflow publishes platform packages before root package to avoid install-time race
- `Cargo.toml` has required crates.io metadata (description, license, repository) and release profile optimizations
- Version sync script derives all package versions from git tag

## Proof Level

- This slice proves: final-assembly
- Real runtime required: no (CI runs remotely; npm publish requires registry access)
- Human/UAT required: no (CI verification is the proof — local validation substitutes)

## Integration Closure

- Upstream surfaces consumed: `opencode-plugin-aft/` plugin code (S06), `Cargo.toml` build config (S01), JSON protocol contract (S01–S05)
- New wiring introduced in this slice: npm platform package resolution in resolver, `@aft/core` as publishable package wrapping the plugin, CI workflow connecting git tags → binary builds → npm publish
- What remains before the milestone is truly usable end-to-end: Milestone-level UAT (agent uses AFT tools in a real OpenCode session) — that's milestone DoD, not slice-level

## Observability / Diagnostics

- **Resolver logging:** `findBinary()` returns the resolved path and logs the resolution source (npm-package, PATH, cargo) at debug level. On failure, the thrown error includes which sources were attempted and why each failed.
- **Platform key inspection:** `platformKey()` is exported and testable — agents can call it directly to verify platform mapping without running the full resolver.
- **Package structure validation:** `scripts/validate-packages.mjs` (T02) provides a single command to verify all 6 package.json files have correct os/cpu fields, version alignment, and optionalDependencies. Until T02, manual inspection of `npm/*/package.json` and `@aft/core` package.json covers this.
- **Failure visibility:** Unsupported platform/arch combinations throw with the specific `process.platform` and `process.arch` values in the error message, enabling immediate diagnosis.
- **Redaction:** No secrets in this slice. Binary paths and platform strings are safe to log.

## Verification

(existing verification plus diagnostic check)

- `bun test opencode-plugin-aft/src/__tests__/resolver.test.ts` — resolver unit tests proving platform mapping, fallback chain, Windows exe handling
- `node scripts/validate-packages.mjs` — validates all 6 package.json files have correct structure, os/cpu fields, version alignment, optionalDependencies
- `actionlint .github/workflows/release.yml` or manual YAML structure review — workflow has all 5 matrix entries, correct build commands, publish ordering
- `cargo build --release` — release binary compiles with optimized profile
- `grep -q '"license"' Cargo.toml` — crates.io required fields present
- Resolver error message on unsupported platform includes `process.platform` and `process.arch` values (verified in unit test)

## Tasks

- [x] **T01: Create npm distribution packages and wire resolver** `est:45m`
  - Why: The npm package structure is the foundation — 5 platform packages for binary distribution, `@aft/core` as the installable root, and the resolver must know how to find binaries from installed platform packages. This is all the code and config that gets published.
  - Files: `npm/darwin-arm64/package.json`, `npm/darwin-x64/package.json`, `npm/linux-arm64/package.json`, `npm/linux-x64/package.json`, `npm/win32-x64/package.json`, `opencode-plugin-aft/package.json`, `opencode-plugin-aft/src/resolver.ts`, `Cargo.toml`, `opencode-plugin-aft/src/__tests__/resolver.test.ts`
  - Do: Create 5 platform package dirs under `npm/` with minimal package.json (name, version, os, cpu, preferUnplugged, bin field). Update `opencode-plugin-aft/package.json` to `@aft/core` with optionalDependencies. Rewrite resolver to check npm platform package first via `require.resolve` (try/catch), then PATH, then cargo. Handle `.exe` suffix for win32. Add Cargo.toml metadata (description, license, repository, keywords) and `[profile.release]` with LTO + codegen-units=1 + strip. Write resolver unit tests covering platform mapping, fallback chain, and Windows naming.
  - Verify: `bun test opencode-plugin-aft/src/__tests__/resolver.test.ts` passes; all 5 platform package.json files exist with correct os/cpu; `@aft/core` package.json has all 5 optionalDependencies; Cargo.toml has description, license, repository fields
  - Done when: All package files exist with correct structure, resolver correctly prioritizes npm package → PATH → cargo, and resolver tests pass

- [x] **T02: Add CI release workflow and version sync** `est:40m`
  - Why: The distribution pipeline needs automation — cross-compile for 5 platforms on tag push, strip binaries, place them in the right platform packages, publish all 6 npm packages in correct order. Plus a version script so the 7 version fields (6 package.json + Cargo.toml) stay in sync from the git tag.
  - Files: `.github/workflows/release.yml`, `scripts/version-sync.mjs`, `scripts/validate-packages.mjs`
  - Do: Write GitHub Actions workflow triggered on `v*` tag push. Matrix: macOS runner for darwin-arm64 (native) + darwin-x64 (cross-target), Ubuntu runner for linux-arm64 + linux-x64 (via `cross` with musl), Windows runner for win32-x64 (native MSVC). Each job: install Rust toolchain, build release binary, strip it, upload as artifact. Final publish job: download all artifacts, place binaries in platform package dirs, run version-sync from tag, npm publish platform packages first then `@aft/core`. Write version-sync script that takes a version string and updates all 7 locations. Write validate-packages script that checks all package.json structures.
  - Verify: `node scripts/validate-packages.mjs` passes; `node scripts/version-sync.mjs 0.2.0 --dry-run` shows correct updates; workflow YAML has 5 build matrix entries and publish job with correct dependency ordering; `cargo build --release` succeeds locally
  - Done when: CI workflow defines the full build→publish pipeline for all 5 platforms, version sync script updates all 7 version locations, package validation script confirms structural correctness

## Files Likely Touched

- `npm/darwin-arm64/package.json`
- `npm/darwin-x64/package.json`
- `npm/linux-arm64/package.json`
- `npm/linux-x64/package.json`
- `npm/win32-x64/package.json`
- `opencode-plugin-aft/package.json`
- `opencode-plugin-aft/src/resolver.ts`
- `opencode-plugin-aft/src/__tests__/resolver.test.ts`
- `Cargo.toml`
- `.github/workflows/release.yml`
- `scripts/version-sync.mjs`
- `scripts/validate-packages.mjs`
