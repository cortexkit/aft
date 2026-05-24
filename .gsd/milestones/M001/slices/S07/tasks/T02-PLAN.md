---
estimated_steps: 6
estimated_files: 3
---

# T02: Add CI release workflow and version sync

**Slice:** S07 — Binary Distribution Pipeline
**Milestone:** M001

## Description

Build the GitHub Actions release workflow that cross-compiles the Rust binary for all 5 platforms on git tag push, then publishes all 6 npm packages in the correct order. Plus utility scripts for version synchronization (single source of truth from git tag) and package structure validation.

## Steps

1. Write `.github/workflows/release.yml` with:
   - Trigger: `push.tags: ['v*']`
   - **Build matrix** (5 jobs, run in parallel):
     - `darwin-arm64`: `macos-latest` runner, native `cargo build --release --target aarch64-apple-darwin`, strip binary
     - `darwin-x64`: `macos-latest` runner, `rustup target add x86_64-apple-darwin`, `cargo build --release --target x86_64-apple-darwin`, strip binary
     - `linux-arm64`: `ubuntu-latest` runner, `cross build --release --target aarch64-unknown-linux-musl`, binary is already stripped (profile.release)
     - `linux-x64`: `ubuntu-latest` runner, `cross build --release --target x86_64-unknown-linux-musl`, binary is already stripped
     - `win32-x64`: `windows-latest` runner, native `cargo build --release --target x86_64-pc-windows-msvc`
   - Each build job: use `dtolnay/rust-toolchain@stable` with target, install `cross` for Linux jobs (pinned v0.2.5), build, upload binary as artifact with platform name
   - **Publish job** (depends on all 5 build jobs):
     - Download all artifacts
     - Copy each binary into the correct `npm/{platform}/bin/` directory
     - Run `node scripts/version-sync.mjs` with version from tag ref
     - `npm publish` each platform package (in parallel or sequence)
     - `npm publish` the `@aft/core` package last
     - Needs `NPM_TOKEN` secret for authentication

2. Write `scripts/version-sync.mjs`: takes version as first arg (or `--from-tag` to read `GITHUB_REF_NAME`), updates version in all 7 locations: 5 platform package.json files, `opencode-plugin-aft/package.json` (both `version` and all `optionalDependencies`), and `Cargo.toml`. Support `--dry-run` flag that prints changes without writing. Validate version format (semver).

3. Write `scripts/validate-packages.mjs`: reads all 6 package.json files, validates:
   - Each platform package has `os`/`cpu` arrays matching its directory name
   - Each platform package has `preferUnplugged: true`
   - Each platform package has a `bin` field
   - `@aft/core` has `optionalDependencies` listing all 5 platform packages
   - All 6 packages have matching versions
   - No missing required fields (name, version)

4. Verify `scripts/validate-packages.mjs` passes against the packages created in T01.

5. Verify `scripts/version-sync.mjs 0.2.0 --dry-run` shows the correct 7 update targets.

6. Verify `cargo build --release` succeeds locally with the release profile from T01.

## Must-Haves

- [ ] CI workflow triggered on `v*` tag push with 5 parallel build jobs + 1 sequential publish job
- [ ] Linux targets use `cross` with musl for static binaries
- [ ] macOS builds both architectures on macos runner (native arm64, cross x64)
- [ ] Windows builds with MSVC on windows runner
- [ ] Publish job places binaries in correct platform dirs and publishes platform packages before `@aft/core`
- [ ] Version sync script updates all 7 version locations from a single version string
- [ ] Package validation script checks structure, os/cpu fields, version alignment

## Verification

- `node scripts/validate-packages.mjs` — passes without errors
- `node scripts/version-sync.mjs 0.2.0 --dry-run` — shows all 7 locations with correct version
- Workflow YAML has correct matrix structure (review: 5 build targets, publish dependency, correct cross/native tooling per platform)
- `cargo build --release` — compiles successfully with LTO and strip

## Inputs

- `npm/*/package.json` — platform package files from T01
- `opencode-plugin-aft/package.json` — `@aft/core` package from T01
- `Cargo.toml` — with metadata from T01
- S07-RESEARCH.md — ripgrep CI patterns, cross pinning, publish ordering, pitfalls

## Observability Impact

- **`scripts/validate-packages.mjs`** — single-command structural health check for all 6 package.json files. Run `node scripts/validate-packages.mjs` to verify os/cpu fields, version alignment, optionalDependencies, and required fields. Exit code 0 = all checks pass; non-zero = prints which checks failed and why.
- **`scripts/version-sync.mjs --dry-run`** — previews all 7 version update targets without writing. Useful for verifying version sync scope before a release.
- **CI workflow failure visibility** — each build job uploads its binary as a named artifact; the publish job logs each `npm publish` command. If a platform build fails, the artifact name tells you which target broke. If publish fails, the NPM_TOKEN or package version conflict is surfaced in the job logs.
- **No new runtime observability** — these are CI/tooling artifacts, not runtime code. Runtime resolver observability was established in T01.

## Expected Output

- `.github/workflows/release.yml` — complete release pipeline
- `scripts/version-sync.mjs` — version synchronization utility
- `scripts/validate-packages.mjs` — package structure validation
