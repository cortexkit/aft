# S07: Binary Distribution Pipeline — Research

**Date:** 2026-03-14

## Summary

S07 delivers the npm distribution pipeline that makes `npm install @aft/core` resolve the correct platform-specific binary for macOS ARM/Intel, Linux ARM/x64, and Windows x64. This follows the well-established pattern used by esbuild, turbo, and oxlint: a root `@aft/core` package declares `optionalDependencies` on 5 platform-specific packages (`@aft/darwin-arm64`, etc.), each containing only the pre-built binary and a `package.json` with `os`/`cpu` fields. npm's native platform filtering installs only the matching package. A GitHub Actions CI workflow cross-compiles the Rust binary for all 5 targets and publishes all 6 npm packages on git tag push.

The existing codebase is well-prepared. The resolver at `opencode-plugin-aft/src/resolver.ts` has an explicit slot for npm platform package resolution. The plugin entry point, bridge, and tool modules are all working and tested. The Cargo.toml needs metadata additions for crates.io publishing (description, license, repository). The release binary is 7.4MB on macOS ARM — reasonable for npm distribution.

The primary technical risk is tree-sitter grammar C compilation during cross-compilation. The `cc` crate handles C cross-compilation well inside `cross`'s Docker containers for Linux targets, and macOS native builds handle both architectures. Windows MSVC handles it natively. This is a well-trodden path — ripgrep, tree-sitter-cli, and other Rust projects with C build deps ship this way.

## Recommendation

Follow the esbuild/turbo pattern exactly:

1. **Platform packages** (`npm/@aft-{platform}/`): Minimal `package.json` with `os`/`cpu` fields + the binary in `bin/aft` (or `bin/aft.exe` for Windows). 5 packages total under the `@aft` scope.

2. **Root package** (`@aft/core`): Contains the plugin TypeScript code (current `opencode-plugin-aft/`), binary resolver, and declares `optionalDependencies` on all 5 platform packages. Has a `bin` stub for direct CLI usage if needed.

3. **Resolver update**: Add npm platform package resolution as the first check in `findBinary()` — use `require.resolve` to find the platform package's binary, falling back to PATH and cargo.

4. **CI workflow**: GitHub Actions matrix build — macOS native for both darwin targets, `cross` for both Linux targets (musl for static linking), Windows native for win32-x64. Tag push triggers build + npm publish.

5. **Cargo.toml metadata**: Add description, license, repository, keywords for `cargo install aft` fallback via crates.io.

Use musl (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) for Linux targets. This produces fully static binaries with zero runtime dependencies — critical for npm distribution where the user's glibc version is unknown.

## Don't Hand-Roll

| Problem | Existing Solution | Why Use It |
|---------|------------------|------------|
| Rust cross-compilation for Linux | `cross-rs/cross` | Docker-based cross-compilation with correct C toolchains. Handles tree-sitter C deps automatically. Pinned version for CI stability. |
| Rust toolchain setup in CI | `dtolnay/rust-toolchain` action | Standard, well-maintained, handles target installation |
| macOS x86_64 from arm64 | Native `cargo build --target x86_64-apple-darwin` | macOS runners can compile for both architectures natively — no Docker/cross needed |
| npm platform filtering | npm `os`/`cpu` fields in platform package.json | npm's built-in mechanism. No postinstall script needed for filtering. |
| Binary resolution at runtime | `require.resolve()` + `process.platform`/`process.arch` | Standard Node.js mechanism to find the installed platform package's binary path |

## Existing Code and Patterns

- `opencode-plugin-aft/src/resolver.ts` — Has explicit comment "Platform-specific npm package (S07 — not yet implemented)" with a clear slot. Resolution order should become: npm platform package → PATH → `~/.cargo/bin/aft`.
- `opencode-plugin-aft/src/index.ts` — Plugin entry point exports a `Plugin` async function. This file becomes the main entry of `@aft/core`.
- `opencode-plugin-aft/src/bridge.ts` — BinaryBridge takes `(binaryPath: string, cwd: string)`. No changes needed — resolver feeds it the path.
- `opencode-plugin-aft/package.json` — Currently named `opencode-plugin-aft`. Will need renaming to `@aft/core` and addition of `optionalDependencies`.
- `Cargo.toml` — Minimal. Needs `[package]` metadata additions and `[profile.release]` optimization settings.
- `target/release/aft` — 7.4MB on macOS ARM. Acceptable for npm platform package size.

## Constraints

- **Tree-sitter grammars compile C code via `cc` crate** — each platform build must have a working C compiler for the target. `cross` Docker images provide this for Linux. macOS and Windows have native C toolchains.
- **npm `optionalDependencies` requires `--no-optional` NOT be set** — if a user installs with `--no-optional`, no platform binary is installed. Resolver must fall back gracefully to PATH/cargo.
- **All 6 npm packages must have identical versions** — root `@aft/core` declares exact version matches for `optionalDependencies`. CI must publish all 6 atomically (or close to it).
- **Platform packages use `@aft` npm scope** — scope must be available on npm. Alternative: unscoped names like `aft-darwin-arm64`.
- **Windows binary must be `.exe`** — resolver needs platform-aware binary name logic.
- **Linux musl builds are fully static** — no dynamic linking issues but musl compilation can be slower and has edge cases with DNS resolution (not relevant for our use case — binary doesn't do network I/O).
- **`cargo install aft` requires crates.io metadata** — description, license, repository fields are mandatory for publishing.

## Common Pitfalls

- **npm publish ordering** — If `@aft/core` publishes before platform packages, installs during the window will fail to resolve the platform binary. Publish platform packages first, then the root package. CI should wait for all 5 platform publishes to succeed before publishing core.
- **Cross/Docker version pinning** — New `cross` releases have historically broken CI (noted in ripgrep's workflow). Pin to a specific version (v0.2.5 is stable and well-tested).
- **macOS x86_64 cross-compile needs target installed** — `rustup target add x86_64-apple-darwin` is required on arm64 runners. The `dtolnay/rust-toolchain` action handles this with the `target` parameter.
- **Windows binary naming** — The resolver must check for `aft.exe` on Windows. `process.platform === 'win32'` is the signal.
- **`require.resolve` may fail in monorepos/pnpm** — pnpm uses symlinks and strict mode. The resolver should try `require.resolve` in a try/catch and fall back. esbuild has special pnp handling, but for v1 a simple try/catch is sufficient.
- **npm `os`/`cpu` fields don't work as `dependencies`** — they only work as `optionalDependencies`. Using regular `dependencies` would fail installation on non-matching platforms.
- **Stripping release binaries** — `strip` reduces binary size significantly (~30-40%). macOS: `strip`, Linux: done inside cross Docker container, Windows: not needed (MSVC strips debug info by default in release).
- **`preferUnplugged: true`** in platform package.json — Required for Yarn PnP compatibility. Without it, Yarn may zip the binary inside `.pnp.cjs` where it can't be executed.

## Open Risks

- **npm scope availability** — `@aft` scope may be taken on npm. Need to verify availability or use an alternative like `@aft-tools` or `@agent-file-tools`. This should be checked before implementation begins.
- **CI build time** — Cross-compilation for 5 targets with tree-sitter C deps could take 10-20 minutes. Matrix parallelism helps, but if builds timeout that's a problem.
- **Linux musl + tree-sitter C compilation** — musl cross-compilation with `cc` crate is well-tested, but tree-sitter grammar C code may use glibc-specific features. Low risk — tree-sitter is widely used with musl — but worth a quick local verification with `cross` if available.
- **Version synchronization across 6 packages** — Manual version bumps across 6 `package.json` files plus `Cargo.toml` are error-prone. CI should derive version from git tag, not from files. A small script to update all versions from a single source would help.

## Skills Discovered

| Technology | Skill | Status |
|------------|-------|--------|
| Rust cross-compilation | `mohitmishra786/low-level-dev-skills@rust-cross` (34 installs) | available |
| GitHub Actions Rust CI | `mohitmishra786/low-level-dev-skills@cargo-workflows` (31 installs) | available |
| npm binary distribution | none relevant found | none found |

Both available skills have low install counts. The esbuild/turbo/ripgrep patterns are well-documented and sufficient — no skill installation recommended.

## Sources

- esbuild platform package structure: `npm view @esbuild/darwin-arm64` — `os: ["darwin"], cpu: ["arm64"]`, binary in `bin/esbuild`, `preferUnplugged: true` (source: npm registry inspection)
- esbuild runtime resolution: `require.resolve(`${pkg}/${subpath}`)` with fallback to downloading directly from npm (source: `/tmp/esbuild-inspect/package/install.js`)
- turbo platform package structure: binary in `bin/turbo`, identical pattern to esbuild (source: `/tmp/turbo-inspect/turbo-darwin-arm64/package.json`)
- ripgrep CI workflow: matrix build with `cross` for Linux, native for macOS/Windows, `cross` version pinned to v0.2.5 (source: `github.com/BurntSushi/ripgrep/.github/workflows/release.yml`)
- tree-sitter C compilation: uses `cc` crate, checks `CC` env vars for cross-compilation (source: `target/debug/build/tree-sitter-typescript-*/output`)
- Current binary size: 7.4MB release on macOS ARM, 12MB debug (source: local build)
- Current resolver has npm slot: explicit comment at line 31-32 of `opencode-plugin-aft/src/resolver.ts`
