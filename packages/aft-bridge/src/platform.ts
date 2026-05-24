/**
 * Shared platform / architecture mappings used by both the resolver and downloader.
 *
 * Keeping them here avoids duplication and ensures resolver + downloader always
 * agree on the canonical platform key strings (e.g. "darwin-arm64").
 */

/**
 * Nested map: `process.platform` → `process.arch` → platform-key string.
 *
 * Used by the resolver to turn the current runtime environment into the
 * canonical key (e.g. `"darwin-arm64"`) that the rest of the system uses.
 *
 * v0.28: Windows ARM64 ships a native binary instead of relying on Windows
 * 11's Prism x64-on-ARM64 emulator. Native ARM64 avoids the emulation tax
 * on ARM-native Node installs and ONNX Runtime ARM64 (auto-downloaded by
 * `onnx-runtime.ts`) couples more cleanly with a same-architecture binary.
 */
export const PLATFORM_ARCH_MAP: Record<string, Record<string, string>> = {
  darwin: { arm64: "darwin-arm64", x64: "darwin-x64" },
  linux: { arm64: "linux-arm64", x64: "linux-x64" },
  win32: { arm64: "win32-arm64", x64: "win32-x64" },
};

/**
 * Flat map: platform-key string → GitHub release asset filename.
 *
 * Used by the downloader to turn the canonical key into the exact asset name
 * that appears in the GitHub release (e.g. `"aft-darwin-arm64"`).
 */
export const PLATFORM_ASSET_MAP: Record<string, string> = {
  "darwin-arm64": "aft-darwin-arm64",
  "darwin-x64": "aft-darwin-x64",
  "linux-arm64": "aft-linux-arm64",
  "linux-x64": "aft-linux-x64",
  "win32-arm64": "aft-win32-arm64.exe",
  "win32-x64": "aft-win32-x64.exe",
};
