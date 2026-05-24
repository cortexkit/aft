/**
 * Per-server GitHub-release configuration for LSP servers AFT can
 * auto-download.
 *
 * Each entry is a templated asset-name pattern keyed by `(platform, arch)`.
 * The installer resolves the latest eligible release tag (with 7-day grace),
 * matches the asset against `assetTemplate(platform, arch, version)`,
 * downloads it, extracts to the cache, and exposes the binary.
 *
 * NOTE: terraform-ls uses HashiCorp's release API, not GitHub. It's a
 * separate code path; not included in this table.
 *
 * NOTE: oxlint is bundled inside the project's `node_modules/@oxlint/*`
 * package via the existing project-node_modules resolution. AFT does not
 * download it independently. Excluded from this table.
 *
 * NOTE: kotlin-language-server is hosted on JetBrains CDN, not GitHub.
 * Skipped from auto-download for v0.17.0 — users install via Homebrew /
 * scoop / Snap / coursier.
 */

export type Platform = "darwin" | "linux" | "win32";
export type Arch = "x64" | "arm64";

/**
 * Asset name template — receives platform/arch (already mapped to the
 * project's naming convention) and the resolved version, returns the
 * literal asset filename to look for in the release `assets[]`.
 *
 * Returns `null` when this platform/arch combo is unsupported.
 */
export type AssetTemplate = (platform: Platform, arch: Arch, version: string) => string | null;

export type ArchiveType = "tar.gz" | "tar.xz" | "zip";

export interface GithubServerSpec {
  /** AFT server-kind id (matches `crates/aft/src/lsp/registry.rs::ServerKind::id_str`). */
  readonly id: string;
  /** GitHub `owner/repo`. */
  readonly githubRepo: string;
  /** Binary name placed under the cache `bin/` dir after extraction. */
  readonly binary: string;
  /** Returns the asset filename and archive type for the given platform/arch + version. */
  readonly resolveAsset: (
    platform: Platform,
    arch: Arch,
    version: string,
  ) => { name: string; archive: ArchiveType } | null;
  /**
   * Path inside the extracted archive where the binary lives, relative to the
   * extraction root. May contain `${version}` placeholder. The installer
   * looks up this path and copies/symlinks it to `bin/<binary>`.
   */
  readonly binaryPathInArchive: (platform: Platform, arch: Arch, version: string) => string;
}

/* ─────────────────────────── helpers ─────────────────────────── */

function exe(platform: Platform, name: string): string {
  return platform === "win32" ? `${name}.exe` : name;
}

/* ─────────────────────────── server definitions ─────────────────────────── */

/**
 * clangd: `clangd-mac-${ver}.zip`, `clangd-linux-${ver}.zip`,
 *         `clangd-windows-${ver}.zip`. Always zip.
 *
 * The release tag is just the version (e.g. "21.1.0"). Asset name uses
 * the version directly, not the tag.
 *
 * Extracted layout: `clangd_${ver}/bin/clangd`.
 */
const CLANGD: GithubServerSpec = {
  id: "clangd",
  githubRepo: "clangd/clangd",
  binary: "clangd",
  resolveAsset: (platform, _arch, version) => {
    const platformName = platform === "darwin" ? "mac" : platform === "linux" ? "linux" : "windows";
    return { name: `clangd-${platformName}-${version}.zip`, archive: "zip" };
  },
  binaryPathInArchive: (platform, _arch, version) =>
    `clangd_${version}/bin/${exe(platform, "clangd")}`,
};

/**
 * lua-language-server: `lua-language-server-${tag}-${platform}-${arch}.${ext}`
 * Tag (= version) is included in the asset name.
 *
 * Extracted layout: `bin/lua-language-server` at archive root.
 */
const LUA_LS: GithubServerSpec = {
  id: "lua-ls",
  githubRepo: "LuaLS/lua-language-server",
  binary: "lua-language-server",
  resolveAsset: (platform, arch, version) => {
    const ext: ArchiveType = platform === "win32" ? "zip" : "tar.gz";
    const platformName =
      platform === "darwin" ? "darwin" : platform === "linux" ? "linux" : "win32";
    const archName = arch === "arm64" ? "arm64" : "x64";
    return {
      name: `lua-language-server-${version}-${platformName}-${archName}.${ext}`,
      archive: ext,
    };
  },
  binaryPathInArchive: (platform, _arch, _version) => `bin/${exe(platform, "lua-language-server")}`,
};

/**
 * zls: `zls-${arch}-${platform}.${ext}`
 * - arch: x86_64 | aarch64
 * - platform: linux | macos | windows
 * - ext: tar.xz on unix, zip on windows
 *
 * Extracted layout: `zls` at archive root.
 */
const ZLS: GithubServerSpec = {
  id: "zls",
  githubRepo: "zigtools/zls",
  binary: "zls",
  resolveAsset: (platform, arch, _version) => {
    const ext: ArchiveType = platform === "win32" ? "zip" : "tar.xz";
    const archName = arch === "arm64" ? "aarch64" : "x86_64";
    const platformName =
      platform === "darwin" ? "macos" : platform === "linux" ? "linux" : "windows";
    return { name: `zls-${archName}-${platformName}.${ext}`, archive: ext };
  },
  binaryPathInArchive: (platform, _arch, _version) => exe(platform, "zls"),
};

/**
 * tinymist: `tinymist-${arch}-${platform-triple}.${ext}`
 * - arch: x86_64 | aarch64
 * - platform-triple: apple-darwin | unknown-linux-gnu | pc-windows-msvc
 * - ext: tar.gz on unix, zip on windows
 *
 * Extracted layout: `tinymist` at archive root.
 */
const TINYMIST: GithubServerSpec = {
  id: "tinymist",
  githubRepo: "Myriad-Dreamin/tinymist",
  binary: "tinymist",
  resolveAsset: (platform, arch, _version) => {
    const archName = arch === "arm64" ? "aarch64" : "x86_64";
    const triple =
      platform === "darwin"
        ? "apple-darwin"
        : platform === "linux"
          ? "unknown-linux-gnu"
          : "pc-windows-msvc";
    const ext: ArchiveType = platform === "win32" ? "zip" : "tar.gz";
    return { name: `tinymist-${archName}-${triple}.${ext}`, archive: ext };
  },
  binaryPathInArchive: (platform, _arch, _version) => exe(platform, "tinymist"),
};

/**
 * texlab: `texlab-${arch}-${platform}.${ext}`
 * - arch: x86_64 | aarch64
 * - platform: linux | macos | windows
 * - ext: tar.gz on unix, zip on windows
 *
 * Extracted layout: `texlab` at archive root.
 */
const TEXLAB: GithubServerSpec = {
  id: "texlab",
  githubRepo: "latex-lsp/texlab",
  binary: "texlab",
  resolveAsset: (platform, arch, _version) => {
    const archName = arch === "arm64" ? "aarch64" : "x86_64";
    const platformName =
      platform === "darwin" ? "macos" : platform === "linux" ? "linux" : "windows";
    const ext: ArchiveType = platform === "win32" ? "zip" : "tar.gz";
    return { name: `texlab-${archName}-${platformName}.${ext}`, archive: ext };
  },
  binaryPathInArchive: (platform, _arch, _version) => exe(platform, "texlab"),
};

export const GITHUB_LSP_TABLE: readonly GithubServerSpec[] = [
  CLANGD,
  LUA_LS,
  ZLS,
  TINYMIST,
  TEXLAB,
];

/** Find an entry by AFT server id. */
export function findGithubServerById(id: string): GithubServerSpec | undefined {
  return GITHUB_LSP_TABLE.find((entry) => entry.id === id);
}

/**
 * Map Node's `process.platform` and `process.arch` to our (Platform, Arch)
 * pair, or null when the host isn't supported.
 */
export function detectHostPlatform(): { platform: Platform; arch: Arch } | null {
  const platform = process.platform;
  if (platform !== "darwin" && platform !== "linux" && platform !== "win32") return null;

  const arch = process.arch;
  if (arch === "x64") return { platform, arch: "x64" };
  if (arch === "arm64") return { platform, arch: "arm64" };
  return null;
}
