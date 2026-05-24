import { homedir, tmpdir } from "node:os";
import { join } from "node:path";

/** `~/.cache/aft/bin/` (or the platform equivalent) — same as plugin's `getCacheDir`. */
export function getAftBinaryCacheDir(): string {
  if (process.env.AFT_CACHE_DIR) {
    return join(process.env.AFT_CACHE_DIR, "bin");
  }
  if (process.platform === "win32") {
    const localAppData = process.env.LOCALAPPDATA || process.env.APPDATA;
    const base = localAppData || join(homedir(), "AppData", "Local");
    return join(base, "aft", "bin");
  }
  const base = process.env.XDG_CACHE_HOME || join(homedir(), ".cache");
  return join(base, "aft", "bin");
}

export function getAftBinaryName(): string {
  return process.platform === "win32" ? "aft.exe" : "aft";
}

/**
 * Root of the LSP package cache populated by the OpenCode/Pi plugin.
 *
 * `~/.cache/aft/lsp-packages/<urlencoded-pkg>/node_modules/.bin/<binary>` for
 * npm-distributed servers (typescript-language-server, pyright, etc.).
 */
export function getAftLspPackagesDir(): string {
  if (process.env.AFT_CACHE_DIR) {
    return join(process.env.AFT_CACHE_DIR, "lsp-packages");
  }
  if (process.platform === "win32") {
    const localAppData = process.env.LOCALAPPDATA || process.env.APPDATA;
    const base = localAppData || join(homedir(), "AppData", "Local");
    return join(base, "aft", "lsp-packages");
  }
  const base = process.env.XDG_CACHE_HOME || join(homedir(), ".cache");
  return join(base, "aft", "lsp-packages");
}

/**
 * Root of the LSP binary cache populated by the OpenCode/Pi plugin.
 *
 * `~/.cache/aft/lsp-binaries/<id>/bin/<binary>` for GitHub-distributed
 * servers (clangd, lua-ls, zls, tinymist, texlab).
 */
export function getAftLspBinariesDir(): string {
  if (process.env.AFT_CACHE_DIR) {
    return join(process.env.AFT_CACHE_DIR, "lsp-binaries");
  }
  if (process.platform === "win32") {
    const localAppData = process.env.LOCALAPPDATA || process.env.APPDATA;
    const base = localAppData || join(homedir(), "AppData", "Local");
    return join(base, "aft", "lsp-binaries");
  }
  const base = process.env.XDG_CACHE_HOME || join(homedir(), ".cache");
  return join(base, "aft", "lsp-binaries");
}

/** Resolve the plugin log file path. Shared with the plugin's logger. */
export function getTmpLogPath(filename: string): string {
  return join(tmpdir(), filename);
}
